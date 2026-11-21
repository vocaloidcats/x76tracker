# Putting the app on GitHub

## 1. Create the repository

1. On github.com, click **+** (top right), then **New repository**.
2. Name it `x76-elo-tracker` and set it to **Public**.
3. Leave "Add a README", ".gitignore" and "license" **unticked**. The project already has them.
4. Click **Create repository**.

## 2. Upload the code

Run these in PowerShell, one at a time. Replace `YOUR-USERNAME` with your GitHub username.

Only the first time you use git on this PC, tell git your name and email (they appear on your commits):

```powershell
git config --global user.name "Your Name"
```

```powershell
git config --global user.email "you@example.com"
```

Then:

```powershell
cd C:\clde\x76-tracker-native
```

```powershell
git init
```

```powershell
git add .
```

```powershell
git commit -m "x76 Elo Tracker 2.0.0"
```

```powershell
git branch -M main
```

```powershell
git remote add origin https://github.com/YOUR-USERNAME/x76-elo-tracker.git
```

```powershell
git push -u origin main
```

The first push opens a browser window so you can sign in to GitHub. `.gitignore` keeps out the `target` build folder (over 1 GB).

## 3. Publish the .exe for the community

1. On the repository page, click **Releases**, then **Draft a new release**.
2. Under **Choose a tag**, type `v2.0.0` and pick **Create new tag**.
3. Title: `x76 Elo Tracker 2.0.0`.
4. Drag `C:\clde\x76-tracker-native\target\release\x76EloTracker.exe` into the "Attach binaries" box.
5. Click **Publish release**.

Share this link: `https://github.com/YOUR-USERNAME/x76-elo-tracker/releases/latest`

## Good to know

- **"Windows protected your PC" warning:** people will see it because the .exe isn't code-signed. They click **More info**, then **Run anyway**.
- **Slint credit:** Slint's free license requires the "Made with Slint" credit, which is already shown at the bottom of the app. Keep it in future versions.
- **Updating later:** change `version` in `Cargo.toml`, run `cargo build --release`, commit and push, then publish a new release with the new .exe.
