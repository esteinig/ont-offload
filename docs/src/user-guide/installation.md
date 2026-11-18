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
cargo install --locked --git https://github.com/esteinig/ont-offload --tag vX.Y.Z ont-offload
```

## As a service

```sh
sudo install -m 0644 packaging/ont-offload.service /etc/systemd/system/
sudo install -D -m 0640 packaging/ont-offload.example.toml /etc/ont-offload/config.toml
# edit the config, place the sentinel on the share, then:
echo "<target_id>" | sudo tee /mnt/nas/ont/.ont-offload-target
ont-offload check-config
sudo systemctl enable --now ont-offload
systemctl status ont-offload     # status line: runs, backlog, outage
```

The service runs `ont-offload watch`; see [Running the daemon](watch.md). `sudo systemctl reload ont-offload` applies a changed `rate_limit`; other configuration changes need `sudo systemctl restart ont-offload`. `ont-offload status` shows the live state, and `ont-offload bench` suggests `workers`, `rate_limit` and `chunk_size` for the site's share ([Tuning with bench](watch.md#tuning-with-bench)).
