use std::fmt::Display;

pub fn format_vec<T: Display>(v: &[T]) -> String {
    let items: Vec<String> = v.iter().map(|it| it.to_string()).collect();
    format!("[{}]", items.join(", "))
}
