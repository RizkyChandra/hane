# Working on hane

A from-scratch vector graphics engine for the browser. Read [`docs/PLAN.md`](docs/PLAN.md) for
the phases and [`docs/DECISION-LOG.md`](docs/DECISION-LOG.md) for the numbered decisions.

## Hard constraints

These are architectural decisions, not preferences. CI enforces all of them.

- **Zero external dependencies (D-001).** Never add anything to `[dependencies]` or
  `[dev-dependencies]` in any crate except `hane-wasm`. No `proptest`, no `criterion`, no
  `rand`. `scripts/check-zero-deps.py` runs *first* in CI and fails the build. Write what you
  need by hand — see `fuzz.rs` for the property-testing harness that replaced `proptest`.
- **No unsafe.** The workspace sets `unsafe_code = "forbid"`. `hane-wasm` is the sole exception,
  because `#[unsafe(no_mangle)]` is required to export anything to JS at all.
- **`hane-gpu` never calls a GL function** (D-010). It computes tile bins, buffer contents and
  draw commands as plain data; `hane-wasm` holds the context and submits them. This keeps the
  parts that contain bugs testable with `cargo test`, without a browser.
- **f64 throughout.** The narrowing to `f32` happens once, at the GPU buffer boundary in
  `hane-gpu`, and nowhere else. A design tool zooms deep enough that `f32` visibly snaps
  control points.
- **Every public item needs a doc comment.** `missing_docs` is a warning and CI runs
  `clippy -D warnings`, so an undocumented public item fails the build.
- **Tests are plain `#[cfg(test)] mod tests`** with `assert!`/`assert_eq!`. No frameworks.

## Gates — all four must pass before you commit

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
python3 scripts/check-zero-deps.py
```

## Numerical facts you will trip over

- **`CubicBez::eval` and `QuadBez::eval` are bit-exact at `t = 0` and `t = 1` only.** The
  Bernstein basis functions do not sum to exactly 1.0 in floating point, so even a constant
  curve drifts by an ulp mid-parameter. Never assume interior exactness.
- **`Point::lerp` and `Vec2::lerp` use the symmetric `(1-t)a + tb` form**, whose weights are
  exactly 1 and 0 at *both* ends. The cheaper `a + (b - a) * t` loses the `t = 1` endpoint
  entirely when the operands differ wildly in magnitude — `lerp(1e300, 1.0, 1.0)` returns
  `0.0`. Subdivision depends on exact endpoints; do not "optimise" this back.
- **Tolerances must be relative** when coordinates can be large. Test curves reach 1e9, where
  an absolute 1e-12 is below a single ulp and no correct implementation can pass.
- **Nothing in `fuzz.rs` may call libm.** `sin`/`cos` are not bit-identical across platforms,
  which would break seed reproducibility. Build transforms from raw coefficients.

## Existing API — `hane-geom`

| Type | Notes |
|---|---|
| `Point`, `Vec2` | Deliberately distinct. `Point - Point = Vec2`, `Point + Vec2 = Point`. |
| `Affine([f64;6])` | SVG `matrix()` coefficient order, so SVG transforms are a copy not a conversion. `inverse() -> Option`, `max_scale()` for pixel→document tolerance. |
| `Rect` | `Rect::EMPTY` is inverted infinities and is the **union identity** — fold `union_point` over it to accumulate bboxes. |
| `QuadBez`, `CubicBez` | `eval`, `deriv_control` (hodograph Bernstein coefficients), `deriv_at`, `deriv2_at`, `split`, `subsegment`, `bounding_box`, `length_at_t`, `t_at_length`. |
| `PathEl` | `MoveTo`/`LineTo`/`QuadTo`/`CurveTo`/`ClosePath`. No arcs — SVG arcs convert to cubics at parse time so downstream handles three segment kinds, not four. |
| `fuzz::{Rng, check}` | Deterministic property testing. `check(name, iters, Rng::cubic, \|c\| ...)`. Failures print a self-contained seed. |

## Branches

**`dev` is the default and the only branch you target.** Open every PR against it.

`main` is the release branch and moves only when a release is cut: `dev` merges into `main`,
a `v*` tag is pushed, and `.github/workflows/release.yml` builds the wasm, bundles the shell
and publishes via GoReleaser. Never push to `main` directly and never target it in a PR.

## Workflow

Work in your own git worktree so parallel agents do not collide:

```sh
git -C /path/to/hane worktree add ../hane-wt-<slug> -b <phase>/<slug>
```

Put new code in a **new file**. Add at most one `mod` line to `lib.rs` — several agents edit
that file in parallel, and it is the only place conflicts happen. Then push and
`gh pr create --repo RizkyChandra/hane --title "..." --body "Closes #N. ..."`.

## Style

- Comments explain **why**, not what. Match the density of the existing code: light, but every
  non-obvious choice justified. A comment naming the failure a line prevents is worth ten
  describing what it does.
- No abstractions with one implementation. No traits, factories or config for a single case.
- No public API beyond what the issue asks for.
- Prefer inherent methods on existing types over new wrapper types; if a module only adds
  inherent methods, it needs `mod x;` and no `pub use`.
- Mark deliberate shortcuts with a `ponytail:` comment naming the ceiling and the upgrade path,
  e.g. `// ponytail: re-integrates from 0 each Newton step; cache a length table when P4 needs
  thousands of dash stops on one curve`.

## When an acceptance criterion looks wrong

Say so rather than bending the implementation to satisfy it. One has already been wrong: "a
quarter-circle approximation matches pi/2 to 1e-6" is unachievable for a *single* cubic, whose
arc length is 1.5710166980738827 — off by 2.2e-4 by construction, an approximation error no
quadrature can fix. Report it, implement the reading that tests the right thing, and explain
the substitution in the PR.
