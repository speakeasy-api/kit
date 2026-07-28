.PHONY: fmt fmt-book fmt-md fmt-code

fmt: fmt-code fmt-book ## Format source code and book

fmt-code: ## Format Rust source code
	cargo fmt --all

fmt-book: fmt-md ## Backwards-compatible alias for markdown formatting

fmt-md: ## Format book markdown files
	git ls-files -z '*.md' | xargs -0 npx prettier --write
