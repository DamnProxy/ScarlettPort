//! `cargo run -p victory-core --example savecheck -- <provider folder>`:
//! read-only check of a game-save provider folder; exit 1 on a fatal finding.
fn main() {
    let Some(root) = std::env::args_os().nth(1) else {
        eprintln!("usage: savecheck <provider folder>");
        std::process::exit(2);
    };
    let findings = victory_core::savecheck::check_provider(std::path::Path::new(&root));
    for f in &findings {
        println!("{} {}: {}", if f.fatal { "FATAL" } else { "note " }, f.path.display(), f.what);
    }
    println!("{} finding(s)", findings.len());
    std::process::exit(i32::from(findings.iter().any(|f| f.fatal)));
}
