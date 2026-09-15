# Native installation on immutable Fedora

For container installation, follow the [Distrobox guide](distrobox.md). If you need to install directly on the base instead, `dnf install` doesn't apply on rpm-ostree. Packages layer into a new deployment that only takes effect after a reboot, and the daemon won't start on its own.

```bash
# 1. Enable rpmfusion (full ffmpeg with libx264)
sudo rpm-ostree install https://mirrors.rpmfusion.org/free/fedora/rpmfusion-free-release-$(rpm -E %fedora).noarch.rpm
sudo systemctl reboot

# 2. After reboot: add the project repo and install
sudo curl --output-dir /etc/yum.repos.d/ --remote-name \
  https://copr.fedorainfracloud.org/coprs/sgtaziz/lian-li-linux/repo/fedora-$(rpm -E %fedora)/sgtaziz-lian-li-linux-fedora-$(rpm -E %fedora).repo
sudo rpm-ostree install lian-li-linux
sudo systemctl reboot

# 3. After reboot: start the daemon
systemctl --user daemon-reload
systemctl --user enable --now lianli-daemon.service
```
