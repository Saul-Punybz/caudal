# Caudal on Windows

`caudal.exe` builds with MSVC on x86_64 Windows. CI builds it on every PR
(`.github/workflows/windows.yml`), runs the unit tests and a smoke test of
the real binary (UDP in, LL-HLS out), and keeps `caudal.exe` as the
`caudal-windows-x86_64` artifact of the run.

Tested: GitHub's `windows-latest` runner (Windows Server). **Not tested
yet:** a Windows 10 desktop, running as a service, multicast on Windows.

## Install

1. Put `caudal.exe` in a folder of its own, e.g. `C:\Caudal\`, with a
   `caudal.toml` next to it (start from `caudal.example.toml`).
2. ffmpeg is only needed for `[transcode]` and `[[omt.pull]]` (which take
   `[transcode] ffmpeg` unless `[omt] ffmpeg` says otherwise), plus some
   `[[omt.output]]` codecs. Get a Windows build (gyan.dev or BtbN) and either
   put its `bin` folder on `PATH` or point the config at it. In TOML a
   Windows path goes in single quotes, so the backslashes stay as they are:

   ```toml
   [transcode]
   ffmpeg = 'C:\ffmpeg\bin\ffmpeg.exe'
   ```
3. Check the config, then the setup:

   ```powershell
   C:\Caudal\caudal.exe check C:\Caudal\caudal.toml
   C:\Caudal\caudal.exe doctor --config C:\Caudal\caudal.toml
   ```
4. Run it: `C:\Caudal\caudal.exe --config C:\Caudal\caudal.toml`. Ctrl-C,
   closing the window, logging off and shutting the machine down all stop
   it cleanly.

## Firewall

Windows Defender Firewall blocks inbound traffic to a new program: UDP from
an encoder or VLC (`[[udp.pull]]`), SRT, RTMP, WebRTC and viewers on the LAN
never arrive. Allow the program once, from an administrator PowerShell:

```powershell
New-NetFirewallRule -DisplayName "Caudal" -Direction Inbound -Program "C:\Caudal\caudal.exe" -Action Allow
```

Outbound (`[[udp.push]]` to a multiplexer, `[[srt.push]]`, restreams) is
allowed by default.

## Run it as a service

Caudal is a console program; a service wrapper starts it at boot, restarts
it if it dies, and stops it with Ctrl-C (a clean shutdown). With
[NSSM](https://nssm.cc), from an administrator prompt:

```powershell
nssm install Caudal C:\Caudal\caudal.exe --config C:\Caudal\caudal.toml
nssm set Caudal AppDirectory C:\Caudal
nssm set Caudal AppStdout C:\Caudal\logs\caudal.log
nssm set Caudal AppStderr C:\Caudal\logs\caudal.log
nssm set Caudal AppRotateFiles 1
nssm set Caudal AppRotateBytes 52428800
nssm start Caudal
```

[WinSW](https://github.com/winsw/winsw) works the same way with an XML file.
Not tested yet on a real machine; the Ctrl-C stop path is the one the
console run above uses.

## Things that differ from Linux

- **No `SIGHUP` reload.** Reload with `POST /api/v1/config/reload` (or the
  dashboard), which works on every platform.
- **WebRTC uses one engine.** Linux spreads WebRTC across threads with
  `SO_REUSEPORT`; Windows has no equivalent, so `[webrtc] threads` above 1
  falls back to one engine on one socket.
- **ffmpeg** (transcode) runs without a console window of its own and is
  ended with `TerminateProcess` when its stream ends.
- **Power settings.** A tower PC that sleeps stops the channel. Set the
  power plan to never sleep (`powercfg /change standby-timeout-ac 0`).
