// A tup vanilla-JS example: zero nix config.
// Dropping this tree in and running `tup run --env nix <project>` auto-activates
// the `node` prologue (nodejs + pnpm).
const sum = Array.from({ length: 10 }, (_, i) => i + 1).reduce((a, b) => a + b, 0);
console.log(`hello from tup node example — sum(1..10) = ${sum}`);
