//! The seeded workload of the crash harness and the model of what it must leave behind.
//!
//! Every operation is a pure function of the seed and its number, so the process that
//! runs it and the process that checks it derive the same statements without talking.

use std::collections::{BTreeMap, BTreeSet};

/// SplitMix64: small, fixed, and the same on every toolchain.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `lo..=hi`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo + 1)
    }
}

pub const ACCOUNTS: i64 = 16;
pub const OPENING_BALANCE: i64 = 1000;

/// The schema and the opening rows. Run to the end before any kill.
pub fn setup_statements() -> Vec<String> {
    let mut s = vec![
        "CREATE TABLE accounts (id INT PRIMARY KEY, balance BIGINT NOT NULL, owner TEXT NOT NULL)".to_string(),
        "CREATE INDEX accounts_balance ON accounts (balance)".to_string(),
        "CREATE TABLE transfers (id SERIAL PRIMARY KEY, src INT NOT NULL REFERENCES accounts(id), \
         dst INT NOT NULL REFERENCES accounts(id), amount BIGINT NOT NULL, opno INT NOT NULL)"
            .to_string(),
        "CREATE INDEX transfers_opno ON transfers (opno)".to_string(),
        "CREATE TABLE notes (opno INT NOT NULL, k INT NOT NULL, body TEXT NOT NULL, PRIMARY KEY (opno, k))".to_string(),
        "CREATE INDEX notes_k ON notes (k)".to_string(),
        "CREATE TABLE ledger (opno INT PRIMARY KEY, kind TEXT NOT NULL)".to_string(),
    ];
    let rows: Vec<String> = (1..=ACCOUNTS)
        .map(|id| format!("({id}, {OPENING_BALANCE}, 'opening')"))
        .collect();
    s.push(format!(
        "INSERT INTO accounts (id, balance, owner) VALUES {}",
        rows.join(", ")
    ));
    s.push("CREATE BRANCH opening AS OF NOW".to_string());
    s
}

/// One operation of the workload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// One transaction: debit, credit, a transfer row and a ledger row.
    Transfer { src: i64, dst: i64, amount: i64 },
    /// One statement inserting `rows` rows.
    Notes { rows: i64 },
    /// One statement renaming the owner of a range of accounts.
    Owner { lo: i64, hi: i64 },
    /// One transaction: delete the notes of an earlier operation, and a ledger row.
    DropNotes { of: u64 },
    /// One transaction that changes rows and is rolled back.
    Abandoned { src: i64, amount: i64 },
}

impl Op {
    pub fn kind(&self) -> &'static str {
        match self {
            Op::Transfer { .. } => "transfer",
            Op::Notes { .. } => "notes",
            Op::Owner { .. } => "owner",
            Op::DropNotes { .. } => "drop_notes",
            Op::Abandoned { .. } => "abandoned",
        }
    }
}

/// Operation number `n` (from 1) of the workload of `seed`.
pub fn op(seed: u64, n: u64) -> Op {
    let mut rng = Rng::new(seed ^ n.wrapping_mul(0xD6E8_FEB8_6659_FD93));
    let account = |rng: &mut Rng| rng.range(1, ACCOUNTS as u64) as i64;
    match rng.range(0, 99) {
        0..=44 => {
            let src = account(&mut rng);
            let mut dst = account(&mut rng);
            if dst == src {
                dst = src % ACCOUNTS + 1;
            }
            Op::Transfer {
                src,
                dst,
                amount: rng.range(1, 250) as i64,
            }
        }
        45..=64 => Op::Notes {
            rows: rng.range(2, 40) as i64,
        },
        65..=79 => {
            let lo = account(&mut rng);
            let hi = (lo + rng.range(1, 9) as i64).min(ACCOUNTS);
            Op::Owner { lo, hi }
        }
        80..=89 => Op::DropNotes {
            of: rng.range(1, n.max(1)),
        },
        _ => Op::Abandoned {
            src: account(&mut rng),
            amount: rng.range(1, 250) as i64,
        },
    }
}

/// The statements of an operation, in order. More than one statement means an explicit
/// transaction whose first is `BEGIN` and whose last is `COMMIT` or `ROLLBACK`.
pub fn statements(n: u64, op: &Op) -> Vec<String> {
    match op {
        Op::Transfer { src, dst, amount } => vec![
            "BEGIN".to_string(),
            format!("UPDATE accounts SET balance = balance - {amount} WHERE id = {src}"),
            format!("UPDATE accounts SET balance = balance + {amount} WHERE id = {dst}"),
            format!("INSERT INTO transfers (src, dst, amount, opno) VALUES ({src}, {dst}, {amount}, {n})"),
            format!("INSERT INTO ledger (opno, kind) VALUES ({n}, 'transfer')"),
            "COMMIT".to_string(),
        ],
        Op::Notes { rows } => {
            let values: Vec<String> = (0..*rows).map(|k| format!("({n}, {k}, 'note {n}/{k}')")).collect();
            vec![format!(
                "INSERT INTO notes (opno, k, body) VALUES {}",
                values.join(", ")
            )]
        }
        Op::Owner { lo, hi } => vec![format!(
            "UPDATE accounts SET owner = 'op{n}' WHERE id BETWEEN {lo} AND {hi}"
        )],
        Op::DropNotes { of } => vec![
            "BEGIN".to_string(),
            format!("DELETE FROM notes WHERE opno = {of}"),
            format!("INSERT INTO ledger (opno, kind) VALUES ({n}, 'drop_notes')"),
            "COMMIT".to_string(),
        ],
        Op::Abandoned { src, amount } => vec![
            "BEGIN".to_string(),
            format!("UPDATE accounts SET balance = balance + {amount} WHERE id = {src}"),
            format!("INSERT INTO ledger (opno, kind) VALUES ({n}, 'abandoned')"),
            "ROLLBACK".to_string(),
        ],
    }
}

/// What the database must hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    /// id -> (balance, owner)
    pub accounts: BTreeMap<i64, (i64, String)>,
    /// opno -> (src, dst, amount)
    pub transfers: BTreeMap<i64, (i64, i64, i64)>,
    /// (opno, k) -> body
    pub notes: BTreeMap<(i64, i64), String>,
    /// opno -> kind
    pub ledger: BTreeMap<i64, String>,
    /// Transfer ids seen so far: a later one must be larger than all of them.
    pub transfer_ids: BTreeSet<i64>,
}

impl Model {
    pub fn opening() -> Self {
        Self {
            accounts: (1..=ACCOUNTS)
                .map(|id| (id, (OPENING_BALANCE, "opening".to_string())))
                .collect(),
            transfers: BTreeMap::new(),
            notes: BTreeMap::new(),
            ledger: BTreeMap::new(),
            transfer_ids: BTreeSet::new(),
        }
    }

    pub fn apply(&mut self, n: u64, op: &Op) {
        let n = n as i64;
        match op {
            Op::Transfer { src, dst, amount } => {
                if let Some(a) = self.accounts.get_mut(src) {
                    a.0 -= amount;
                }
                if let Some(a) = self.accounts.get_mut(dst) {
                    a.0 += amount;
                }
                self.transfers.insert(n, (*src, *dst, *amount));
                self.ledger.insert(n, "transfer".to_string());
            }
            Op::Notes { rows } => {
                for k in 0..*rows {
                    self.notes.insert((n, k), format!("note {n}/{k}"));
                }
            }
            Op::Owner { lo, hi } => {
                for id in *lo..=*hi {
                    if let Some(a) = self.accounts.get_mut(&id) {
                        a.1 = format!("op{n}");
                    }
                }
            }
            Op::DropNotes { of } => {
                let of = *of as i64;
                self.notes.retain(|(opno, _), _| *opno != of);
                self.ledger.insert(n, "drop_notes".to_string());
            }
            Op::Abandoned { .. } => {}
        }
    }

    pub fn total(&self) -> i64 {
        self.accounts.values().map(|a| a.0).sum()
    }
}
