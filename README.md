> **This is a Wine/Proton compatibility pass** on [open.mp launcher](https://github.com/openmultiplayer/launcher)
> (v1.6.1), for running it on Linux. Every change is in [WINE-BUILD.md](WINE-BUILD.md):
> what broke under Wine, why, and the fix. The in-game server browser also needs
> [`ompoverlay.asi`](https://github.com/akacross/omp-ingame-browser-wine) in the game
> folder; the whole setup (prefix, WebView2, launch script) is in
> [samp-openmp-linux](https://github.com/akacross/samp-openmp-linux). Outside Wine the
> launcher behaves as upstream's. The app keeps open.mp's name and branding, as its
> license requires. Builds: [Releases](../../releases), or the `Wine build` workflow.

# open.mp launcher

Made with Tauri + React-Native ❤️

# Usage:

Use open.mp launcher to enjoy a live, reliable, and populated server list to find any server you want to play on!  
Just download it from [Releases](https://github.com/openmultiplayer/launcher/releases/latest) page and run it!

### Important Note:
For users using old Windows 10, Windows 8(.1), or Windows 7, if you do not have WebView2 installed (comes preinstalled in newer Windows 10 and 11 setups, and comes with Microsoft Edge), You must install this launcher using **setup** file in releases section.  

# Development

### For all OSes:

- Install [nightly version](https://rust-lang.github.io/rustup/concepts/channels.html) of rust toolchain
- Install [NodeJS](https://nodejs.org/en/download) and `npm` (or `yarn` or anything else)  
  **Note**: Please make sure you are not using node v20.6, anything else, lower or higher, should work fine.
- Clone repository:

```bash
git clone https://github.com/openmultiplayer/launcher
```

- Prepare for running:

```bash
cd launcher
yarn # or any other way you use to install dependecies using your installed package manager
yarn start
```

- For building a release version, you can use:

```bash
yarn tauri build
```

# Donations

While open.mp is a totally free project and everyone in the team dedicates their time on this project to provide the best for the community, we can still use whatever contribution by anyone, to cover our costs or motivate developers.  
Here is our donation link you can use to show your generosity!

- **https://opencollective.com/openmultiplayer**
