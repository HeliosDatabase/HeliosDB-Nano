**Feature-truth tests: every configuration key, command-line flag, shell command and REST route is enumerated from the product itself and must either be shown to do something or be on a list of known gaps.**

A key, flag, command or route could be accepted and then ignored, and nothing failed.

Four tests enumerate what the product accepts (the keys of the configuration, the flags in `--help` of every subcommand, the shell's command registry, the routes in `src/api/routes/`) and compare it with lists in `tests/feature_truth/`: configuration keys nothing acts on, flags that are accepted and ignored, flags and keys no test sets, shell commands that say "not yet implemented" or answer with fixed text, REST routes that answer 501 or whose handler says it is unfinished. Each entry names the work package that owns it. The tests fail when something new is accepted without a test or a list entry, and when a listed gap is closed without its line being removed, so the lists can only shrink on purpose.

Tests: `tests/feature_truth_config.rs`, `tests/feature_truth_cli.rs`, `tests/feature_truth_repl.rs`, `tests/feature_truth_rest.rs`.
