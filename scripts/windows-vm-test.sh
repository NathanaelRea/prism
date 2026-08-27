#!/usr/bin/env bash
set -euo pipefail

usage() {
	cat <<'EOF'
Usage: scripts/windows-vm-test.sh [options]

Run Prism's native Windows checks in the installed Omarchy Windows VM.

Options:
  --bootstrap-only  Install/verify the guest toolchain and SSH access, then exit.
  --check-only      Run windows-check.ps1 without the real-tool platform smoke.
  --keep-running    Do not stop a VM that this command started.
  -h, --help        Show this help.

The first run opens a temporary RDP window to install the Windows toolchain and
OpenSSH. Later runs are headless and use the dedicated SSH key stored under the
user state directory. The VM is stopped on exit only when this command started it.
EOF
}

bootstrap_only=0
check_only=0
keep_running=0
while (($#)); do
	case "$1" in
	--bootstrap-only) bootstrap_only=1 ;;
	--check-only) check_only=1 ;;
	--keep-running) keep_running=1 ;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		printf 'unknown option: %s\n\n' "$1" >&2
		usage >&2
		exit 2
		;;
	esac
	shift
done

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

require_command() {
	if ! command -v "$1" >/dev/null 2>&1; then
		printf 'missing required command: %s\n' "$1" >&2
		exit 1
	fi
}

for command in git nc ps scp ssh ssh-keygen tar; do
	require_command "$command"
done

if [[ $(uname -s) != Linux ]]; then
	printf 'scripts/windows-vm-test.sh requires the Linux host running the Omarchy Windows VM\n' >&2
	exit 1
fi
if [[ ! -x /usr/bin/omarchy-windows-vm ]]; then
	printf 'Omarchy Windows VM command is unavailable: /usr/bin/omarchy-windows-vm\n' >&2
	exit 1
fi

state_root="${XDG_STATE_HOME:-$HOME/.local/state}/prism/windows-vm"
bridge="$state_root/rdp-bridge"
key="$state_root/id_ed25519"
known_hosts="$state_root/known_hosts"
lock_file="$state_root/run.lock"
credentials="$HOME/.config/windows/credentials"
mkdir -p "$bridge"
chmod 700 "$state_root" "$bridge"

if command -v flock >/dev/null 2>&1; then
	exec 9>"$lock_file"
	if ! flock -n 9; then
		printf 'another Windows VM test is already running (%s)\n' "$lock_file" >&2
		exit 1
	fi
fi

if [[ ! -f $key ]]; then
	ssh-keygen -q -t ed25519 -N '' -C 'prism-windows-vm' -f "$key"
fi
chmod 600 "$key"

read_credential() {
	local wanted="$1" name value
	[[ -f $credentials ]] || return 1
	while IFS='=' read -r name value; do
		if [[ $name == "$wanted" ]]; then
			printf '%s' "$value"
			return 0
		fi
	done <"$credentials"
	return 1
}

rdp_ready() {
	nc -z -w 1 127.0.0.1 3389 >/dev/null 2>&1
}

container_ip() {
	ps -ww -eo args= | awk '
		$1 ~ /(^|\/)docker-proxy$/ {
			proto = host_ip = host_port = container_ip = container_port = ""
			for (i = 1; i <= NF; i++) {
				if ($i == "-proto") proto = $(i + 1)
				if ($i == "-host-ip") host_ip = $(i + 1)
				if ($i == "-host-port") host_port = $(i + 1)
				if ($i == "-container-ip") container_ip = $(i + 1)
				if ($i == "-container-port") container_port = $(i + 1)
			}
			if (proto == "tcp" && host_ip == "127.0.0.1" && host_port == "3389" && container_port == "3389") {
				print container_ip
				exit
			}
		}'
}

started_vm=0
rdp_pid=
archive=
file_list=
cleanup() {
	local status=$?
	trap - EXIT INT TERM
	if [[ -n ${rdp_pid:-} ]]; then
		kill "$rdp_pid" 2>/dev/null || true
		wait "$rdp_pid" 2>/dev/null || true
	fi
	if [[ -n ${archive:-} ]]; then
		rm -f "$archive"
	fi
	if [[ -n ${file_list:-} ]]; then
		rm -f "$file_list"
	fi
	if ((started_vm == 1 && keep_running == 0)); then
		printf '\n==> Stop Omarchy Windows VM\n'
		if ! /usr/bin/omarchy-windows-vm stop; then
			printf 'warning: the Windows VM may still be running\n' >&2
			((status == 0)) && status=1
		fi
	fi
	exit "$status"
}
trap cleanup EXIT INT TERM

if ! rdp_ready; then
	require_command pkexec
	printf '==> Start Omarchy Windows VM headlessly\n'
	printf "    This uses Omarchy's validated internal wait action because no public headless start exists.\n"
	pkexec /usr/bin/omarchy-windows-vm __priv up_wait
	started_vm=1
fi

for _ in $(seq 1 60); do
	rdp_ready && break
	sleep 1
done
if ! rdp_ready; then
	printf 'Windows VM did not expose RDP on 127.0.0.1:3389\n' >&2
	exit 1
fi

vm_ip="$(container_ip)"
if [[ -z $vm_ip ]]; then
	printf 'could not discover the dockurr/windows container address from Docker proxy state\n' >&2
	exit 1
fi
printf '==> Windows VM container: %s\n' "$vm_ip"

vm_user="$(read_credential USERNAME || true)"
if [[ -z $vm_user ]]; then
	printf 'Windows VM username is unavailable in %s\n' "$credentials" >&2
	exit 1
fi

ssh_options=(
	-i "$key"
	-o BatchMode=yes
	-o ConnectTimeout=5
	-o IdentitiesOnly=yes
	-o "UserKnownHostsFile=$known_hosts"
	-o StrictHostKeyChecking=accept-new
)
scp_options=(
	-i "$key"
	-o BatchMode=yes
	-o ConnectTimeout=5
	-o IdentitiesOnly=yes
	-o "UserKnownHostsFile=$known_hosts"
	-o StrictHostKeyChecking=accept-new
)

ssh_ready() {
	ssh "${ssh_options[@]}" "$vm_user@$vm_ip" 'cmd.exe /d /c echo prism-vm-ssh-ready' 2>/dev/null |
		tr -d '\r' |
		grep -qx 'prism-vm-ssh-ready'
}

guest_bootstrap_ready() {
	ssh "${ssh_options[@]}" "$vm_user@$vm_ip" \
		'cmd.exe /d /c if exist C:\PrismVm\bootstrap.complete echo prism-vm-bootstrap-ready' \
		2>/dev/null |
		tr -d '\r' |
		grep -qx 'prism-vm-bootstrap-ready'
}

bootstrap_guest_over_ssh() {
	printf '==> Resume Windows toolchain bootstrap over SSH\n'
	ssh "${ssh_options[@]}" "$vm_user@$vm_ip" \
		'cmd.exe /d /c if not exist prism-vm\bootstrap mkdir prism-vm\bootstrap'
	scp "${scp_options[@]}" \
		scripts/windows-vm-bootstrap.ps1 \
		scripts/install-windows-smoke-tools.ps1 \
		"$key.pub" \
		"$vm_user@$vm_ip:prism-vm/bootstrap/"
	ssh "${ssh_options[@]}" "$vm_user@$vm_ip" \
		'powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File prism-vm\bootstrap\windows-vm-bootstrap.ps1 -Bridge prism-vm\bootstrap'
}

stop_rdp() {
	if [[ -n ${rdp_pid:-} ]]; then
		kill "$rdp_pid" 2>/dev/null || true
		wait "$rdp_pid" 2>/dev/null || true
		rdp_pid=
	fi
}

bootstrap_guest() {
	local password title window command status last_status deadline timeout_seconds
	require_command xdotool
	require_command xfreerdp3
	if [[ -z ${DISPLAY:-} ]]; then
		printf 'the first Windows VM bootstrap needs an X display for its temporary RDP session\n' >&2
		return 1
	fi

	password="$(read_credential PASSWORD || true)"
	if [[ -z $password ]]; then
		printf 'Windows VM password is unavailable in %s\n' "$credentials" >&2
		return 1
	fi

	install -m 600 scripts/windows-vm-bootstrap.ps1 "$bridge/windows-vm-bootstrap.ps1"
	install -m 600 scripts/install-windows-smoke-tools.ps1 "$bridge/install-windows-smoke-tools.ps1"
	install -m 600 "$key.pub" "$bridge/id_ed25519.pub"
	rm -f "$bridge/bootstrap.status" "$bridge/bootstrap.log" "$bridge/rdp.log"

	if [[ ! -f $state_root/krb5.conf ]]; then
		printf '[libdefaults]\n  dns_lookup_kdc = false\n  dns_lookup_realm = false\n' >"$state_root/krb5.conf"
		chmod 600 "$state_root/krb5.conf"
	fi

	title="PrismVmBootstrap$$"
	printf '==> Bootstrap Windows toolchain through a temporary RDP session\n'
	KRB5_CONFIG="$state_root/krb5.conf" \
		printf '%s\n' "$password" |
		KRB5_CONFIG="$state_root/krb5.conf" xfreerdp3 \
			/u:"$vm_user" \
			/v:127.0.0.1:3389 \
			/from-stdin:force \
			/cert:ignore \
			/sec:nla \
			/drive:runner,"$bridge" \
			/title:"$title" \
			/size:1024x768 \
			-audio -microphone -clipboard \
			/log-level:WARN \
			>"$bridge/rdp.log" 2>&1 &
	rdp_pid=$!

	window=
	for _ in $(seq 1 120); do
		window="$(xdotool search --name "$title" 2>/dev/null | tail -1 || true)"
		[[ -n $window ]] && break
		kill -0 "$rdp_pid" 2>/dev/null || break
		sleep 0.25
	done
	if [[ -z $window ]]; then
		printf 'FreeRDP bootstrap window did not appear\n' >&2
		tail -80 "$bridge/rdp.log" >&2 || true
		return 1
	fi

	sleep 3
	xdotool windowactivate --sync "$window"
	xdotool key --clearmodifiers --window "$window" Super_L+r
	sleep 1
	command='powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "\\tsclient\runner\windows-vm-bootstrap.ps1" -Bridge "\\tsclient\runner"'
	xdotool type --clearmodifiers --window "$window" --delay 2 "$command"
	xdotool key --window "$window" Return
	sleep 1
	xdotool windowminimize "$window" 2>/dev/null || true

	timeout_seconds="${PRISM_WINDOWS_VM_BOOTSTRAP_TIMEOUT:-5400}"
	deadline=$((SECONDS + timeout_seconds))
	last_status=
	while ((SECONDS < deadline)); do
		if [[ -f $bridge/bootstrap.status ]]; then
			status="$(tr -d '\r\000' <"$bridge/bootstrap.status")"
			if [[ $status != "$last_status" ]]; then
				printf '    %s\n' "$status"
				last_status="$status"
			fi
			case "$status" in
			PASS*) break ;;
			FAIL*)
				tail -120 "$bridge/bootstrap.log" >&2 || true
				return 1
				;;
			esac
		fi
		if ! kill -0 "$rdp_pid" 2>/dev/null; then
			printf 'FreeRDP exited before Windows bootstrap completed\n' >&2
			tail -80 "$bridge/rdp.log" >&2 || true
			return 1
		fi
		sleep 5
	done
	if [[ ${status:-} != PASS* ]]; then
		printf 'Windows bootstrap timed out after %s seconds\n' "$timeout_seconds" >&2
		tail -120 "$bridge/bootstrap.log" >&2 || true
		return 1
	fi

	for _ in $(seq 1 60); do
		ssh_ready && break
		sleep 2
	done
	if ! ssh_ready; then
		printf 'Windows bootstrap passed, but SSH key authentication is unavailable\n' >&2
		return 1
	fi
	stop_rdp
	printf '==> Windows VM bootstrap PASS\n'
}

if ! ssh_ready; then
	bootstrap_guest
fi
if ! ssh_ready; then
	printf 'could not establish SSH access to the Windows VM\n' >&2
	exit 1
fi
printf '==> Windows VM SSH ready\n'

if ! guest_bootstrap_ready; then
	bootstrap_guest_over_ssh
fi
if ! guest_bootstrap_ready; then
	printf 'Windows VM toolchain bootstrap did not produce its completion marker\n' >&2
	exit 1
fi
printf '==> Windows VM toolchain ready\n'

if ((bootstrap_only == 1)); then
	exit 0
fi

archive="$state_root/source.$$.tar"
file_list="$state_root/source.$$.files"
: >"$file_list"
while IFS= read -r -d '' path; do
	if [[ -e $path || -L $path ]]; then
		printf '%s\0' "$path" >>"$file_list"
	fi
done < <(git ls-files -z --cached --others --exclude-standard)

printf '==> Package current worktree, including uncommitted files\n'
tar -C "$repo_root" --null --verbatim-files-from --no-recursion -T "$file_list" -cf "$archive"
rm -f "$file_list"
file_list=

printf '==> Transfer current worktree to Windows\n'
ssh "${ssh_options[@]}" "$vm_user@$vm_ip" 'cmd.exe /d /c if not exist prism-vm mkdir prism-vm'
scp "${scp_options[@]}" \
	"$archive" \
	scripts/windows-vm-run.ps1 \
	"$vm_user@$vm_ip:prism-vm/"
rm -f "$archive"
archive=

remote_args=(
	pwsh.exe
	-NoLogo
	-NoProfile
	-ExecutionPolicy Bypass
	-File 'prism-vm\windows-vm-run.ps1'
	-Archive "prism-vm\\source.$$.tar"
)
if ((check_only == 1)); then
	remote_args+=(-CheckOnly)
fi

printf '==> Run native Windows tests\n'
# These fixed arguments are intentionally interpreted by remote cmd.exe.
# shellcheck disable=SC2029
ssh "${ssh_options[@]}" "$vm_user@$vm_ip" "${remote_args[*]}"
