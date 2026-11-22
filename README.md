# x76tracker

A Windows app (download `x76tracker.exe` from the [Releases](../../releases/latest) page). It tracks your **points and Elo** on the [x76.gg](https://x76.gg/leaderboard/) Quake Live and R5Reloaded ladders, live, for any players you pick. No login.

> Unofficial and community-made. Not affiliated with x76.gg.

## Use it

1. Run `x76tracker.exe`. Nothing to install.
2. Search your name under **Quake Live** or **R5Reloaded** and click **Track**.
3. Keep the window open while you play. Closing it stops tracking.

What you get:
- **Points and Elo:** your change for Today, Last 24h or All tracked.
- **Ladder position:** your rank and tier.
- **Wins and losses** for the period.
- **Chart:** points or Elo over time; hover it for exact values.
- **Results list:** the elo and points change of each result.
- **Live updates:** cards light up green or red when something changes.

## Options

```
x76tracker.exe --data-dir D:\x76            keep history somewhere else
x76tracker.exe --screenshot out.png [--size 1440x1000] [--light|--dark] [--hover x,y] [--offline]
                                            draw the window into a PNG without showing it
```

## Build

You need [Rust](https://rustup.rs) and Microsoft's C++ Build Tools. Then run:

```
cargo build --release
```

This produces `target\release\x76tracker.exe`.

| File | What it is |
|---|---|
| `src/tracker.rs` | Downloads the ladders, saves change history, search. |
| `src/view.rs` | Turns history into cards, charts and status lines (Today / Last 24h / All tracked). |
| `src/main.rs` | Window, button handlers, the background check loop, single-instance check. |
| `ui/app.slint` | The UI, styled after x76.gg: Archivo and IBM Plex Mono, paper/ink colours, tier colours. |

The app draws its window with Slint's software renderer. Slint's GPU renderer placed some letters a pixel off the line.

## Licensing notes

- **The app:** MIT, see `LICENSE`.
- **Slint:** used under its free Royalty-free License for desktop apps. That license requires a visible Slint credit; the app shows "Made with Slint" at the bottom of the window.
- **Fonts:** Archivo and IBM Plex Mono are under the SIL Open Font License; the license files are in `ui/fonts/`.
