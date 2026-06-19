#!/usr/bin/env bash
set -euo pipefail

source ./functions.sh

PROGRAMS="swipl"

# Find out where the swipl "home dir" is to mount it
SWIPL_HOME_DIR=$(swipl --home)

(exec bwrap \
      --dir /usr \
      --dir /usr/bin \
      --dir /lib \
      --ro-bind ${SWIPL_HOME_DIR} /lib/swipl \
      --dir /tmp \
      --dir /tmp/session \
      --dir /var \
      --symlink ../tmp var/tmp \
      $(generate-binds ${PROGRAMS}) \
      --proc /proc \
      --dev /dev \
      --chdir /tmp/session \
      --unshare-all \
      --share-net \
      --die-with-parent \
      --dir /run/user/$(id -u) \
      --clearenv \
      --setenv XDG_RUNTIME_DIR "/run/user/`id -u`" \
      --setenv PS1 "bwrap-demo$ " \
      --setenv PATH "/usr/bin" \
      --setenv SWI_HOME_DIR "/lib/swipl" \
      --file 11 /etc/passwd \
      --file 12 /etc/group \
      swipl) \
    11< <(getent passwd $UID 65534) \
    12< <(getent group $(id -g) 65534) \
