# OpenWrt IPv6 Watch

This service runs on a Linux host and performs the IPv6 probe locally. SSH is used only for the optional OpenWrt WAN6 recovery operation and wrapper provisioning.

The local probe uses an explicit Linux interface for both DNS and IPv6 ping. DNS is queried with a socket bound to that interface, and `ping6` is invoked with the same interface. If `ping6` is unavailable, it falls back to `ping -6`. This avoids the default resolver and route being redirected through a TUN/proxy interface. The probe target remains fixed as `openwrt.org`; Telegram input cannot change it.

On startup, the program can still use the system `ssh` executable to write the fixed OpenWrt wrapper through SSH stdin; it does not use `scp`. The wrapper is only needed for the remote `recover` operation.

When `install_wrapper_on_startup = true`, the program writes `/usr/local/sbin/ipv6-watch-ssh-command` through SSH and applies mode `0755` automatically. The startup SSH account must temporarily be able to execute the fixed remote installation command. A key already restricted with `command="..."` cannot perform this bootstrap step; after installing the wrapper, set `install_wrapper_on_startup = false` and use the restricted key:

```text
restrict,command="/usr/local/sbin/ipv6-watch-ssh-command" ssh-ed25519 AAAA...
```

For Dropbear, use the supported equivalent `command="..."` restriction and verify that forwarding and interactive shell access are disabled by the deployed version.

When the Telegram polling loop starts, the bot registers these commands with Telegram:

```text
/status   获取本地指定网口的 IPv6 状态，并执行一次本地 DNS + IPv6 ping
/recover  通过 SSH 在 OpenWrt 执行 recover，等待后再次执行本地检测
```

Both commands require a configured `allowed_user_ids` user and the configured `chat_id`. `/status` reports the local ping exit code, duration, and stderr when the probe fails. The monitor sends a failure notification after `failure_threshold` consecutive failures, repeats it at `reminder_interval_seconds`, and sends a recovery notification after `recovery_success_threshold` consecutive successes.

## Linux deployment

The repository includes a systemd unit at [`systemd/openwrt-ipv6-watch.service`](systemd/openwrt-ipv6-watch.service).
The following example installs the service under `/usr/local/bin` and keeps configuration and SSH credentials under `/etc/openwrt-ipv6-watch`.

Build the release binary on the target Linux host, or copy a binary built for the target architecture:

```sh
cargo build --release
sudo install -D -m 0755 target/release/openwrt-ipv6-watch \
  /usr/local/bin/openwrt-ipv6-watch
```

Create a dedicated service account and directories:

```sh
sudo useradd --system --home-dir /var/lib/openwrt-ipv6-watch \
  --create-home --shell /usr/sbin/nologin ipv6-watch
sudo install -d -o root -g ipv6-watch -m 0750 /etc/openwrt-ipv6-watch
sudo install -d -o ipv6-watch -g ipv6-watch -m 0750 /var/lib/openwrt-ipv6-watch
```

Install the configuration and credentials:

```sh
sudo install -o root -g ipv6-watch -m 0640 config.example.toml \
  /etc/openwrt-ipv6-watch/config.toml
sudo install -o ipv6-watch -g ipv6-watch -m 0600 \
  /path/to/id_ed25519 /etc/openwrt-ipv6-watch/id_ed25519
sudo install -o ipv6-watch -g ipv6-watch -m 0640 \
  /path/to/known_hosts /etc/openwrt-ipv6-watch/known_hosts
```

Create the Telegram token environment file. The variable name must match `bot_token_env` in the TOML file:

```sh
sudo sh -c 'printf "%s\n" "TELEGRAM_BOT_TOKEN=replace-with-real-token" \
  > /etc/openwrt-ipv6-watch/telegram.env'
sudo chown root:root /etc/openwrt-ipv6-watch/telegram.env
sudo chmod 0600 /etc/openwrt-ipv6-watch/telegram.env
```

Edit `/etc/openwrt-ipv6-watch/config.toml`, especially `probe.interface`, `probe.dns_server`, `chat_id`, `allowed_user_ids`, SSH key paths, and the fixed command values `probe`/`recover`. Telegram uses an explicit 10-second connection timeout and a 35-second HTTP request timeout around the default 20-second long poll. Keep `request_timeout_seconds` greater than `poll_timeout_seconds`.

Install and start the unit:

```sh
sudo install -m 0644 systemd/openwrt-ipv6-watch.service \
  /etc/systemd/system/openwrt-ipv6-watch.service
sudo systemctl daemon-reload
sudo systemctl enable --now openwrt-ipv6-watch.service
sudo systemctl status openwrt-ipv6-watch.service
```

View logs and follow service output:

```sh
sudo journalctl -u openwrt-ipv6-watch.service -f
```

The service runs as the unprivileged `ipv6-watch` user. It only needs read access to the configuration, private key, and `known_hosts`. Password bootstrap requires `sshpass` on the host; Debian/Ubuntu users can install it with `sudo apt-get install openssh-client sshpass ca-certificates`.

For a foreground smoke test before enabling systemd:

```sh
TELEGRAM_BOT_TOKEN='...' \
  /usr/local/bin/openwrt-ipv6-watch /etc/openwrt-ipv6-watch/config.toml
```

## Docker deployment

The repository also includes [`Dockerfile`](Dockerfile). GitHub Actions publishes a `linux/amd64` image to GHCR on pushes to `main` and on version tags such as `v1.2.3`:

```text
ghcr.io/OWNER/REPOSITORY:latest
ghcr.io/OWNER/REPOSITORY:main
ghcr.io/OWNER/REPOSITORY:1.2.3
```

The package may be private by default. Log in to GHCR first if the repository package is not public:

```sh
echo "$GITHUB_TOKEN" | docker login ghcr.io -u GITHUB_USERNAME --password-stdin
docker pull ghcr.io/OWNER/REPOSITORY:latest
```

For password authentication, remove or comment out `private_key` in the copied TOML and set:

```toml
[openwrt]
username = "root"
password_env = "OPENWRT_SSH_PASSWORD"
install_wrapper_on_startup = true
known_hosts = "/etc/openwrt-ipv6-watch/known_hosts"
```

Create the mounted configuration directory and an env file. Neither the SSH password nor the Telegram token is built into the image:

```sh
sudo install -d -m 0755 /etc/openwrt-ipv6-watch
sudo cp config.example.toml /etc/openwrt-ipv6-watch/config.toml
sudo cp /path/to/known_hosts /etc/openwrt-ipv6-watch/known_hosts
sudo chmod 0644 /etc/openwrt-ipv6-watch/known_hosts

sudo sh -c 'printf "%s\n" \
  "TELEGRAM_BOT_TOKEN=replace-with-real-token" \
  "OPENWRT_SSH_PASSWORD=replace-with-openwrt-password" \
  > /etc/openwrt-ipv6-watch/telegram.env'
sudo chmod 0600 /etc/openwrt-ipv6-watch/telegram.env
```

Run the container:

```sh
docker run -d \
  --name openwrt-ipv6-watch \
  --restart unless-stopped \
  --network host \
  --cap-add NET_RAW \
  --env-file /etc/openwrt-ipv6-watch/telegram.env \
  -v /etc/openwrt-ipv6-watch:/etc/openwrt-ipv6-watch:ro \
  ghcr.io/OWNER/REPOSITORY:latest
```

Check its logs:

```sh
docker logs -f openwrt-ipv6-watch
```

The Docker image contains `iputils-ping`, `iproute2` (including the `ip` command), `openssh-client`, and `sshpass`. `--network host` exposes the host interfaces inside the container, and `CAP_NET_RAW` permits interface-bound DNS sockets and ICMPv6. Password mode first writes the fixed wrapper through SSH, then uses SSH only for fixed `recover`; the password is passed to `sshpass` through `SSHPASS`, never as a command-line argument. SSH key authentication with the OpenWrt forced-command key remains the safer option. If password authentication is enabled for `root`, configure OpenWrt separately so that this account cannot obtain an unrestricted interactive shell. For an already-installed forced-command key, set `install_wrapper_on_startup = false`.

The local probe target is fixed as `openwrt.org`. The `recover_remote_command` setting is validated at startup and must be exactly `recover`.
