#[varnish::vmod]
mod lambda {
    /// Example function: returns true if a number is even
    pub fn is_even(n: i64) -> bool {
        n % 2 == 0
    }

    /// Example function: doubles a number
    pub fn double(n: i64) -> i64 {
        n * 2
    }

    /// Example function: concatenates a string with itself
    pub fn repeat(s: &str) -> String {
        format!("{}{}", s, s)
    }
}

#[cfg(test)]
varnish::run_vtc_tests!("tests/*.vtc");
