"""A tup Python example: zero nix config.

Dropping this tree in and running `tup run --env nix <project>` auto-activates
the `python` prologue (python3 + uv + ruff).
"""


def main() -> None:
    total = sum(range(1, 11))
    print(f"hello from tup python example — sum(1..10) = {total}")


if __name__ == "__main__":
    main()
