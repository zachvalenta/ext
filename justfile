# run dev mode
run *ARGS:
    cargo run -- {{ARGS}}

# build bin to project root
build:
    cargo build --release
    cp target/release/ext ./ext

# build bin and install to ~/Documents/denv/bin
install:
    cargo build --release
    cp target/release/ext /Users/zach/Documents/denv/bin/ext
