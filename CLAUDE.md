# CLAUDE.md

## Changelog

`CHANGELOG.md` is the record of changes (Keep a Changelog format); GitHub
Releases copy their notes from it.

Every user-visible change adds an entry under `## [Unreleased]` in the same
commit or PR, filed under Added, Changed, or Fixed. When users must act after
updating (rerun `setup.sh`, reconnect, edit SSH config), add or update the
upgrade subsection under Unreleased.

## Releasing

Run these steps when the user asks for a release. Each step ends when its check
passes.

1. **Choose the version.** While on 0.x, bump the minor version when Unreleased
   has Added or Changed entries, otherwise the patch version.
2. **Gate.** On an up-to-date, clean `main`, run every command in `README.md`'s
   `## Development` section. Check: all pass. Run `cargo test` as written, in a
   debug build: `--release` turns off overflow checks and has hidden failing
   tests before.
3. **Bump the version** in `Cargo.toml` and `herdr-plugin.toml`, then run
   `cargo check` without `--locked` to update `Cargo.lock` (`--locked` rejects
   the bump). Check: `git diff --stat` lists exactly these three files.
4. **Move Unreleased into the release.** Move the entries under
   `## [Unreleased]` into a new `## [X.Y.Z] - YYYY-MM-DD` section dated today,
   leaving `## [Unreleased]` empty above it. In the links at the bottom, point
   `[Unreleased]` at `compare/vX.Y.Z...HEAD` and add
   `[X.Y.Z]: https://github.com/kosuketut/herdr-remotedownloder/compare/vPREVIOUS...vX.Y.Z`.
   Check: Unreleased has no entries and the `[X.Y.Z]` link names the previous
   tag.
5. **Commit, tag, push.** Commit the four files on `main` as
   `chore: release vX.Y.Z`, then create an annotated tag and push both:

   ```sh
   git tag -a vX.Y.Z -m "Herdr Remote File Transfer vX.Y.Z"
   git push origin main vX.Y.Z
   ```

   Check: `git ls-remote --tags origin vX.Y.Z` shows the tag.
6. **Publish the GitHub Release** with the CHANGELOG section as its notes:

   ```sh
   awk -v v="X.Y.Z" 'index($0, "## [" v "]") == 1 {p=1; next} p && (/^## \[/ || /^\[[^]]+\]: /) {exit} p' CHANGELOG.md \
     | gh release create vX.Y.Z --title vX.Y.Z --notes-file -
   ```

   Check: `gh release view vX.Y.Z` shows the same entries as the CHANGELOG
   section, and the compare links for `[Unreleased]` and `[X.Y.Z]` resolve.
