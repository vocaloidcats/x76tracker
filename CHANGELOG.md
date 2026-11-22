# Changelog

Each version's section is shown in the app (What's new) and used as its GitHub release notes. Pictures are paths in this repository; the moving ones are animated PNGs.

## 2.2.2

![A share card: banner, avatar and name, then each game's rank emblem, tier, rank, points, Elo and a 48-hour sparkline](docs/images/share-card.png)

### New

- **Share card:** a picture of your x76 profile and ranks for Discord or anywhere else: your banner and avatar, and for each game the rank emblem, tier, rank, points, Elo and a sparkline of your last 48 hours. Press Share card on your profile, then Save PNG. It's saved to Pictures\x76tracker.
- **What's new:** this menu. Every version's changes with pictures, sorted into new, improved and fixed. It opens by itself the first time a new version starts, and from Settings → Updates any time.
  ![The What's new menu](docs/changelog/2.2.2-whats-new@2x.png)
- **Update check:** x76tracker asks GitHub for the latest release when it starts and every 6 hours. When there's a new version, an Update button glows in the title bar and opens its notes here.
  ![The Update button glowing in the title bar](docs/changelog/2.2.2-update@2x.png)
- **Automatic updates:** a new version downloads in the background, is checked against the SHA-256 published with its release, and replaces the old one when you close x76tracker. Turn this off in Settings → Updates to install with one click instead.

### Improved

- **Upsets:** point at an upset you won and a wave of colour washes through its rainbow, then settles back.
  ![An upset win: a wave washes through its rainbow badge](docs/changelog/2.2.2-upset@2x.png)
- **Supporter badge:** the same wave washes through it when you point at it.
  ![A wave washing through the supporter badge](docs/changelog/2.2.2-supporter@2x.png)
- **Panels:** Settings, results, the share card and this menu fade and rise into place, and the Servers tab's bars ease to new player counts.

## 2.2.1

![A fight from the game log: the opponent, their tier, rank and Elo, and your chance of winning on paper](docs/images/result-detail-dark.png)

### New

- **R5Reloaded opponents switch:** Settings → General can add the game-log patch to R5Reloaded, or take it out, so anyone can see who they fought, with the opponent's Elo, upsets and best wins. The original game file is kept as a backup, and the patch is put back if a game update replaces it.
  ![Settings, General: Rich Presence, your players and the R5Reloaded opponents switch](docs/images/settings-general-dark.png)
- **Game folder:** if R5Reloaded isn't in the usual R5R Library folder, you can enter its folder in Settings.

## 2.2.0

Released together with 2.2.1.

![The Servers tab: players on now, today's peak and a chart for each game, then every server](docs/images/servers-dark.png)

### New

- **Servers tab:** live player counts on x76's Quake Live and R5Reloaded servers, with totals, today's peak, a 7-day chart and every server.
- **Settings:** a new Settings window (the sliders button in the title bar), where you pick whose numbers Discord shows.
- **Discord Rich Presence:** while R5Reloaded or Quake Live is running, your Discord status shows your tier, rank, Elo and last 48 hours.
- **Discord profile widget:** if your Discord app already has a profile widget, x76tracker fills its User Data fields: stat tiles, cards with your x76 rank emblem, or a progress bar to your next tier with a 48-hour summary.
  ![A Discord profile widget filled by x76tracker](docs/images/discord-widget.png)
- **Verified builds:** releases are built by GitHub Actions straight from the repository and carry a signed build attestation, so you can check a download came from this code.

### Fixed

- **Closing the window:** it always quits the app now. Before, it could keep running hidden after a dropdown had been opened.
