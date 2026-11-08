//! Independent seed and mutation model; expected values never come from either DB.
use super::cells::{int, text, Cell, Rows};

pub(super) const REGIONS: [&str; 5] = ["East", "West", "North", "South", "Central"];
pub(super) const PRODUCT_CATEGORIES: [&str; 5] = ["Electronics", "Books", "Clothing", "Food", "Sports"];
pub(super) const CATEGORY_NAMES: [&str; 20] = [
    "Electronics",
    "Books",
    "Clothing",
    "Food",
    "Sports",
    "Phones",
    "Laptops",
    "Tablets",
    "Fiction",
    "NonFiction",
    "Shirts",
    "Pants",
    "Shoes",
    "Snacks",
    "Drinks",
    "Soccer",
    "Tennis",
    "Swimming",
    "Camping",
    "Cycling",
];

#[derive(Clone)]
pub(super) struct Customer {
    pub id: i64,
    pub name: String,
    pub age: i64,
    pub region: String,
    pub bio: String,
}
impl Customer {
    pub fn cells(&self) -> Vec<Cell> {
        vec![
            int(self.id),
            text(&self.name),
            text(format!("c{}@test.com", self.id)),
            int(self.age),
            text(&self.region),
            text(format!(
                "{{\"tier\":\"{}\"}}",
                if self.id % 3 == 0 { "gold" } else { "silver" }
            )),
            text(&self.bio),
        ]
    }
}
pub(super) struct Product {
    pub id: i64,
    pub name: String,
    pub category: String,
    pub price: i64,
}
impl Product {
    pub fn cells(&self) -> Vec<Cell> {
        vec![
            int(self.id),
            text(&self.name),
            text(&self.category),
            int(self.price),
            text(format!("Description {}", self.id)),
        ]
    }
}
pub(super) struct Order {
    pub id: i64,
    pub customer: i64,
    pub total: i64,
}
impl Order {
    pub fn cells(&self) -> Vec<Cell> {
        vec![
            int(self.id),
            int(self.customer),
            text(format!("2024-{:02}-{:02}", self.id % 12 + 1, self.id % 28 + 1)),
            text(["pending", "shipped", "delivered", "cancelled"][self.id as usize % 4]),
            int(self.total),
        ]
    }
}
pub(super) struct Item {
    pub id: i64,
    pub order: i64,
    pub product: i64,
    pub quantity: i64,
    pub unit: i64,
}
impl Item {
    pub fn cells(&self) -> Vec<Cell> {
        vec![
            int(self.id),
            int(self.order),
            int(self.product),
            int(self.quantity),
            int(self.unit),
        ]
    }
}
pub(super) struct Model {
    pub customers: Vec<Customer>,
    pub products: Vec<Product>,
    pub orders: Vec<Order>,
    pub items: Vec<Item>,
}

impl Model {
    pub fn new(orders_updated: bool) -> Self {
        Self {
            customers: (1i64..=200)
                .map(|id| Customer {
                    id,
                    name: format!("Customer_{id}"),
                    age: 18 + id % 62,
                    region: REGIONS[id as usize % 5].into(),
                    bio: format!("Bio for customer {id}"),
                })
                .collect(),
            products: (1i64..=50)
                .map(|id| Product {
                    id,
                    name: format!("Product_{id}"),
                    category: PRODUCT_CATEGORIES[id as usize % 5].into(),
                    price: 10 + id % 990,
                })
                .collect(),
            orders: (1i64..=500)
                .map(|id| Order {
                    id,
                    customer: id % 200 + 1,
                    total: if orders_updated && id <= 302 { 2 } else { 50 + id % 950 },
                })
                .collect(),
            items: (1i64..=1000)
                .map(|id| Item {
                    id,
                    order: id % 500 + 1,
                    product: id % 50 + 1,
                    quantity: 1 + id % 10,
                    unit: 5 + id % 100,
                })
                .collect(),
        }
    }
    pub fn categories() -> Rows {
        CATEGORY_NAMES
            .iter()
            .enumerate()
            .map(|(i, name)| {
                vec![
                    int(i as i64 + 1),
                    text(*name),
                    if i < 5 { Cell::Null } else { int(i as i64 % 5 + 1) },
                ]
            })
            .collect()
    }
    pub fn table(&self, name: &str) -> Rows {
        match name {
            "customers" => self.customers.iter().map(Customer::cells).collect(),
            "products" => self.products.iter().map(Product::cells).collect(),
            "orders" => self.orders.iter().map(Order::cells).collect(),
            "order_items" => self.items.iter().map(Item::cells).collect(),
            "categories" => Self::categories(),
            _ => panic!("unknown model table {name}"),
        }
    }
}

pub(super) fn inserted_single() -> Rows {
    (0i64..302)
        .map(|i| vec![int(10000 + i), text(format!("v{i}"))])
        .collect()
}
pub(super) fn inserted_multi() -> Rows {
    (0i64..302)
        .flat_map(|i| (0i64..10).map(move |j| vec![int(20000 + i * 10 + j), text(format!("b{j}"))]))
        .collect()
}
pub(super) fn inserted_select() -> Rows {
    (0i64..302)
        .flat_map(|i| (1i64..=10).map(move |j| vec![int(30000 + i * 10 + j), text(format!("Customer_{j}"))]))
        .collect()
}
pub(super) fn upserted() -> Rows {
    (0i64..30)
        .map(|i| vec![int(i), text("orig"), int(if i < 2 { 11 } else { 10 })])
        .collect()
}
