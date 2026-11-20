//! gangli — placeholder entry point while the build and release pipeline is set up.
//! See docs/ARCHITECTURE.md for what this binary will become.

const BUILD: &str = match option_env!("GANGLI_BUILD") {
    Some(b) => b,
    None => "dev",
};

fn version() -> String {
    format!("gangli 0.0.{BUILD} ({}-{})", std::env::consts::OS, std::env::consts::ARCH)
}

fn main() {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        None | Some("--version") | Some("-V") | Some("version") => println!("{}", version()),
        Some(other) => {
            eprintln!("unknown command: {other}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_names_the_binary() {
        assert!(super::version().starts_with("gangli 0.0."));
    }
}
