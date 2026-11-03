# Privacy policy: AirFrameXR for Meta Quest

Last updated: 3 November 2026

**In short: this app does not collect, store, sell or send any personal data to us or to anyone else.** It shows a video stream
that comes from your own PC, and sends your headset and controller movements back to that same PC, on your own network.

## Who we are

AirFrameXR is an independent open-source project (source: <https://github.com/jfbobier/airframexr-metaquest>). This app is the
Meta Quest client of the AirFrameXR PC streamer (<https://github.com/jfbobier/AirFrameXR>). It is a fork of
[ALVR](https://github.com/alvr-org/ALVR), is not affiliated with the ALVR project, and has no server, account, advertising or
analytics of its own.

## What the app does with data

| Data | What happens | Where it goes |
|---|---|---|
| Video and audio of your game | Received from your PC, decoded and shown; nothing is recorded or kept | Comes from your PC; stays in the headset's memory only while playing |
| Head, controller and hand positions, button presses | Read from the headset and sent to your PC so the game can react | Only to the PC you paired with, over your local network |
| Microphone | Sent to your PC **only if you turn on microphone streaming in the streamer's settings** | Only to your PC |
| Eye, face and body tracking | Read **only if you enable them in the streamer's settings and grant the permission on the headset** | Only to your PC |
| Headset name, IP address | Shown on the welcome panel; the app announces itself on your local network so your PC can find it | Local network only |

The app makes no connection to the internet. It contains no analytics, advertising, tracking or crash-reporting code, and it
never sends data to the developers or to any third party.

## What is stored on the headset

A small settings file in the app's private storage holds: the list of PCs you paired with (names and security keys), the headset's
network name, and the floor-height offset you chose on the welcome panel. It never leaves the headset. **Uninstalling the app
deletes it.**

## Permissions the app requests, and why

- **Network, Wi-Fi state, Wi-Fi lock, wake lock:** to connect to your PC and keep the connection steady while you play.
- **Microphone:** to send your voice to the PC if you enable it in the streamer.
- **Hand, eye, face and body tracking:** to send tracking data to the PC if you enable it in the streamer. Without that, these are
  not read.
- **OpenXR:** to display VR content through the Quest's runtime.

## Your PC

What the AirFrameXR streamer on your PC does with the data it receives (for example the log files it can write when you turn on
logging) is described in its own documentation. Those files stay on your PC unless you choose to share them.

## Children

The app has no accounts and collects nothing from anyone, including children. Age limits follow Meta's rules for the Quest and
the Meta Horizon Store.

## Meta

The Quest and the Meta Horizon Store are run by Meta, which has its own data practices. Those are governed by Meta's privacy
policy and are outside this app.

## Changes

If this policy changes, the new version is published at this address with a new date.

## Contact

Questions about privacy: open an issue at <https://github.com/jfbobier/airframexr-metaquest/issues>.
