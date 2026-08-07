spec ?= packaging/fedora/lian-li-linux.spec

srpm:
	dnf install -y rpm-build cargo npm nodejs git rsync curl >/dev/null
	OUTDIR=$(outdir) bash packaging/fedora/build-srpm.sh
