//! Diagnostic: print the D-Bus signatures produced by candidate Rust types.
//! Run with: cargo test --test signature -- --nocapture

use zbus::zvariant::Type;

#[test]
fn print_signatures() {
    println!("Vec<Vec<i32>>     = {}", <Vec<Vec<i32>> as Type>::SIGNATURE);
    println!("Vec<(Vec<i32>,)>  = {}", <Vec<(Vec<i32>,)> as Type>::SIGNATURE);
    println!("(Vec<i32>,)       = {}", <(Vec<i32>,) as Type>::SIGNATURE);
    println!("Vec<i32>          = {}", <Vec<i32> as Type>::SIGNATURE);
}
