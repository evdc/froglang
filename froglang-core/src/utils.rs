use std::fmt::Display;

pub fn format_vec<T: Display>(v: &Vec<T>) -> String {
    let mut s = "[".to_string();
    for (i, it) in v.iter().enumerate() {
        if i == 0 {
            s = format!("{}{}", s, it);
        } else {
            s = format!("{}, {}", s, it);
        }
        
    }
    format!("{}]", s)
}
