install:
	mkdir -p $(HOME)/.local/bin
	cargo install --path . --locked --root $(HOME)/.local

test:
	cargo test --locked
