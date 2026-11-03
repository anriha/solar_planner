# Pi deployment (Debian trixie, `konazumi@100.64.0.9`)

## Unit

```
just install-unit                                   # copy the unit, enable --now
ssh -t konazumi@100.64.0.9 'sudo loginctl enable-linger konazumi'
```

## Logs: make them survive a reboot

On 2026-11-03 the feed died at 17:47 and the Pi held **no** record of it:
journald here is volatile (`Storage=auto`, but `/var/log/journal/<machine-id>`
never existed), and `konazumi` is not in `systemd-journal`, so
`journalctl --user-unit solar-bridge` answers "No journal files were found".
Fix it once, as root:

```
sudo usermod -aG systemd-journal konazumi
sudo systemd-tmpfiles --create --prefix /var/log/journal
sudo systemctl restart systemd-journald
```

Re-login (the group is applied to new sessions), then:

```
journalctl --user-unit solar-bridge -n 50
journalctl --user-unit solar-bridge --since "2026-11-04 13:00"
journalctl --disk-usage          # should grow past the volatile-only 8 MB
ls /var/log/journal/$(cat /etc/machine-id)/   # should now exist
```

## What the bridge logs

One line per connection lifecycle (`[server] peer: connected as conn N`,
`closed`, `writer stopped`), per subscriber state change (`[state] conn N is
behind`, `caught up`, `reaping`), per link transition (`[bus] link up after N
failed attempt(s)`, plus every failure), and one startup line (`[main]`).
Transitions only — never one line per frame.
