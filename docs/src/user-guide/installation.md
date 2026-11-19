# Installation

## Release binaries

Download the archive for your platform from the [releases page](https://github.com/esteinig/ont-offload/releases), check it against `SHA256SUMS`, and install the binary:

```sh
sha256sum -c SHA256SUMS --ignore-missing
tar -xzf ont-offload-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
sudo install -m 0755 ont-offload-vX.Y.Z-x86_64-unknown-linux-gnu/ont-offload /usr/local/bin/
```

## From source

```sh
cargo install --locked --features tui,report --git https://github.com/esteinig/ont-offload --tag vX.Y.Z ont-offload
```

`--features tui` adds the [monitoring dashboard](tui.md) and `--features report` the PDF rendering of [reports](reports.md) (typst and its fonts are compiled in; nothing else to install); leave them out for a binary without them, e.g. `--features tui,report` for both. The release binaries include both. Without `report`, `ont-offload report` writes the JSON records only.

## As a service

[Deploying on a sequencer](deployment.md) is the full procedure: the service user, share permissions, checks, log rotation, the parallel run and go-live. In short:

```sh
sudo install -m 0644 packaging/ont-offload.sysusers /usr/lib/sysusers.d/ont-offload.conf && sudo systemd-sysusers
sudo install -m 0644 packaging/ont-offload.service /etc/systemd/system/
sudo install -m 0644 packaging/ont-offload.logrotate /etc/logrotate.d/ont-offload
sudo install -D -m 0640 -g ont-offload packaging/ont-offload.example.toml /etc/ont-offload/config.toml
# edit the config, place the sentinel on the share, then:
echo "<target_id>" | sudo tee /mnt/nas/ont/.ont-offload-target
sudo -u ont-offload ont-offload check-config
sudo systemctl enable --now ont-offload
systemctl status ont-offload     # status line: runs, backlog, outage
```
