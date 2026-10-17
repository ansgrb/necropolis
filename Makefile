.PHONY: up down migrate test lint demo build

up:
	docker compose up -d postgres

down:
	docker compose down

build:
	cargo build --workspace

migrate:
	cargo run -p necro-cli -- migrate up

migrate-roundtrip:
	cargo run -p necro-cli -- migrate roundtrip

test:
	cargo test --workspace

lint:
	cargo clippy --workspace --all-targets -- -D warnings

demo: up
	cargo run -p necro-cli -- demo
