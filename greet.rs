/// Greets a person by their name
/// 
/// # Arguments
/// 
/// * `name` - A string slice representing the person's name
/// 
/// # Returns
/// 
/// A String containing the greeting message
pub fn greet(name: &str) -> String {
    format!("Hello, {}!", name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_greet() {
        assert_eq!(greet("Alice"), "Hello, Alice!");
        assert_eq!(greet("Bob"), "Hello, Bob!");
    }
}
