# md-preview systemd user service

This directory contains a **template** systemd user-unit file for running
`md-preview` as an always-on daemon that starts at boot (even without an active
login session).

> **IMPORTANT — these steps are for the HUMAN (CHAIR) to run manually.**
> The installing agent must NOT and did NOT execute `systemctl`, `loginctl`,
> or any other privileged tool.  The unit file is committed only as a template;
> it is NOT installed, enabled, or started by any CI/CD or agent step.

---

## Prerequisites

- `md-preview` binary installed at `~/.local/bin/md-preview` (adjust the path
  in the unit if you installed elsewhere — see `ExecStart` in the `.service`
  file).
- systemd ≥ 240 with user-session support (`systemctl --user` works for your
  account).

---

## Install steps (run these yourself in a terminal)

```sh
# 1. Copy the unit to the user systemd directory.
mkdir -p ~/.config/systemd/user/
cp packaging/systemd/md-preview.service ~/.config/systemd/user/

# 2. Enable lingering so the user service starts at boot/reboot,
#    even without an active login session.
loginctl enable-linger $USER

# 3. Reload the systemd user manager so it picks up the new unit,
#    then enable and start the service.
systemctl --user daemon-reload
systemctl --user enable --now md-preview

# 4. Verify it's running.
systemctl --user status md-preview
```

---

## Useful commands

```sh
# View live logs.
journalctl --user -u md-preview -f

# Stop the service (it will restart automatically unless disabled).
systemctl --user stop md-preview

# Disable autostart and stop.
systemctl --user disable --now md-preview

# Disable lingering (revert boot-start behavior).
loginctl disable-linger $USER
```

---

## Notes

- `Restart=always` / `RestartSec=2`: if the daemon crashes or is killed,
  systemd waits 2 seconds and restarts it.  This is the "always-on" guarantee.
- `WantedBy=default.target`: the unit is activated as part of the user's
  default session target, which is what `--user enable` hooks into.
- The browser-tab WS auto-reconnect (Wave 7) means any open tab will
  automatically reconnect after a daemon restart with capped exponential
  backoff — no manual page reload needed.
