// A tiny but real Rust program: greet a few users and count them.
struct User {
    name: String,
    age: u32,
}

impl User {
    fn greeting(&self) -> String {
        format!("Hello, {}! You are {} years old.", self.name, self.age)
    }
}

fn main() {
    let users = vec![
        User { name: String::from("Ada"), age: 36 },
        User { name: String::from("Linus"), age: 54 },
        User { name: String::from("Grace"), age: 85 },
    ];

    for user in &users {
        println!("{}", user.greeting());
    }

    println!("Total users: {}", users.len());
}
