# Running Under systemd

This repo includes example systemd units for running the backend API and forwarding relayer as separate services.

Files:

- `deploy/systemd/forwarding-backend.service`
- `deploy/systemd/forwarding-relayer.service`
- `deploy/systemd/backend.env.example`
- `deploy/systemd/relayer.env.example`

The units assume:

- the binary is installed at `/usr/local/bin/forwarding-relayer`
- runtime state lives under `/var/lib/forwarding-relayer`
- environment files live under `/etc/forwarding-relayer`
- both services run as user/group `forwarding-relayer`

## Install

```bash
sudo useradd --system --home /var/lib/forwarding-relayer --shell /usr/sbin/nologin forwarding-relayer
sudo mkdir -p /var/lib/forwarding-relayer /etc/forwarding-relayer
sudo chown -R forwarding-relayer:forwarding-relayer /var/lib/forwarding-relayer /etc/forwarding-relayer

sudo install -m 0755 ./target/release/forwarding-relayer /usr/local/bin/forwarding-relayer
sudo install -m 0644 deploy/systemd/forwarding-backend.service /etc/systemd/system/forwarding-backend.service
sudo install -m 0644 deploy/systemd/forwarding-relayer.service /etc/systemd/system/forwarding-relayer.service
sudo install -m 0640 deploy/systemd/backend.env.example /etc/forwarding-relayer/backend.env
sudo install -m 0640 deploy/systemd/relayer.env.example /etc/forwarding-relayer/relayer.env
```

Edit `/etc/forwarding-relayer/backend.env` and `/etc/forwarding-relayer/relayer.env` before starting the services.

The relayer env file must include a real `PRIVATE_KEY_HEX`, and the URLs must match your Celestia gRPC and backend deployment.

## Start

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now forwarding-backend.service
sudo systemctl enable --now forwarding-relayer.service
```

## Inspect

```bash
systemctl status forwarding-backend.service
systemctl status forwarding-relayer.service
journalctl -u forwarding-backend.service -f
journalctl -u forwarding-relayer.service -f
```
