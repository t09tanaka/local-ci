pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(add(1, 2), 3);
    }

    #[test]
    fn adds_negative() {
        assert_eq!(add(-1, -2), -3);
    }

    #[test]
    #[ignore]
    fn slow_one() {
        assert_eq!(add(0, 0), 0);
    }

    mod nested {
        use super::*;

        #[test]
        fn zero() {
            assert_eq!(add(0, 1), 1);
        }
    }
}
