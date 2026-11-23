fn main() {
    // A tup Rust example: zero nix config — dropping this tree in and running
    // `tup run --env nix <project>` auto-activates the `rust` prologue (cargo + rustc).
    let sum: u32 = (1..=10).sum();
    println!("hello from tup rust example — sum(1..=10) = {sum}");
}
