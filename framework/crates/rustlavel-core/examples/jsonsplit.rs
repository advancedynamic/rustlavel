//! Where the time in a JSON response actually goes: building the value, or
//! writing it out.
use rustlavel_core::Json;
use std::time::Instant;

fn build() -> Json {
    Json::Array(
        (1..=100i64)
            .map(|id| {
                Json::object([
                    ("id", Json::from(id as f64)),
                    ("name", Json::from(format!("User {id}"))),
                    ("email", Json::from(format!("user{id}@example.test"))),
                    ("active", Json::Bool(id % 2 == 0)),
                    ("score", Json::from(id as f64 * 1.5)),
                ])
            })
            .collect(),
    )
}

fn main() {
    let rounds = 20_000;

    let t = Instant::now();
    for _ in 0..rounds {
        std::hint::black_box(build());
    }
    let building = t.elapsed().as_micros() as f64 / 1000.0;

    let tree = build();
    let t = Instant::now();
    for _ in 0..rounds {
        std::hint::black_box(tree.to_string());
    }
    let writing = t.elapsed().as_micros() as f64 / 1000.0;

    println!("building the Json tree: {building:8.1} ms");
    println!("writing it out:         {writing:8.1} ms");
    println!("building is {:.0}% of the total", 100.0 * building / (building + writing));
}
