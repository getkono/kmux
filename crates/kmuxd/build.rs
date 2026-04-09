fn main() {
    let alacritty = cfg!(feature = "backend-alacritty");
    let termwiz = cfg!(feature = "backend-termwiz");

    if alacritty && termwiz {
        println!(
            "cargo::error=Only one terminal backend may be enabled at a time. \
             Found both `backend-alacritty` and `backend-termwiz`. \
             Use --no-default-features --features <backend> to pick one."
        );
        std::process::exit(1);
    }
}
