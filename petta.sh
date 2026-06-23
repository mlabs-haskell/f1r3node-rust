#!/usr/bin/env bash
set -euo pipefail

source ./functions.sh

# Programs to run in the sandbox
PROGRAMS="swipl"

# Find out where the swipl "home dir" is to mount it
SWIPL_HOME_DIR=$(swipl --home)

# Options for re-creating the swipl home under sandbox. We can't just bind the
# whole directory because we need to create the symlink
# `/lib/swipl/lib/x86_64-linux -> /lib`.
SWIPL_HOME_BINDS="
  --dir /lib/swipl
  --ro-bind ${SWIPL_HOME_DIR}/ABI /lib/swipl/ABI
  --ro-bind ${SWIPL_HOME_DIR}/app /lib/swipl/app
  --ro-bind ${SWIPL_HOME_DIR}/boot /lib/swipl/boot
  --ro-bind ${SWIPL_HOME_DIR}/boot.prc /lib/swipl/boot.prc
  --ro-bind ${SWIPL_HOME_DIR}/cmake /lib/swipl/cmake
  --ro-bind ${SWIPL_HOME_DIR}/customize /lib/swipl/customize
  --ro-bind ${SWIPL_HOME_DIR}/demo /lib/swipl/demo
  --ro-bind ${SWIPL_HOME_DIR}/doc /lib/swipl/doc
  --ro-bind ${SWIPL_HOME_DIR}/include /lib/swipl/include
  --dir ${SWIPL_HOME_DIR}/lib
  --ro-bind ${SWIPL_HOME_DIR}/library /lib/swipl/library
  --ro-bind ${SWIPL_HOME_DIR}/swipl.home /lib/swipl/swipl.home
"

# Shared libraries that come with the SWI-Prolog distribution. We ignore
# libjpl because it is not needed and it has cyclic dependencies, hanging
# dependency discovery code.
SWIPL_LIBS=$(find ${SWIPL_HOME_DIR}/lib/x86_64-linux -type f | grep -v libjpl)

# Location of PeTTa interpreter
PETTA_DIR=./PeTTa
# Location of Python3 libraries
PYTHON3_LIBDIR=$(echo -e "import sysconfig\nprint(sysconfig.get_config_var('LIBDIR'))" | python3)
# Metta program to run
PROGRAM_FILE=$1
# Session path (simply the CWD of swipl)
SESSION_PATH="/tmp/session"
# Goal
GOAL="assertz(silent(true)), working_directory(_, '${SESSION_PATH}'), assertz(working_dir('${SESSION_PATH}')), load_metta_file('program.metta', Results), use_module(library(json)), json_write_dict(current_output, #{results:Results})."

(exec bwrap \
      --dir /bin \
      --dir /usr \
      --dir /usr/share \
      --dir /usr/bin \
      --dir /lib \
      --dir /tmp \
      --dir ${SESSION_PATH} \
      --dir ${SESSION_PATH}/program \
      --dir /var \
      ${SWIPL_HOME_BINDS} \
      --ro-bind ${PETTA_DIR} /lib/PeTTa \
      --ro-bind ${PROGRAM_FILE} /tmp/session/program.metta \
      --ro-bind ${TERMINFO} /lib/terminal/terminfo \
      --ro-bind ${TERMINFO_DIRS} /usr/share/terminfo \
      $(generate-binds-exes ${PROGRAMS}) \
      $(generate-binds-libs ${SWIPL_LIBS}) \
      --symlink ../tmp var/tmp \
      --symlink /usr/bin/sh /bin/sh \
      --symlink /lib /lib/swipl/lib/x86_64-linux \
      --proc /proc \
      --dev /dev \
      --chdir /tmp/session \
      --unshare-all \
      --share-net \
      --die-with-parent \
      --dir /run/user/$(id -u) \
      --clearenv \
      --setenv XDG_RUNTIME_DIR "/run/user/`id -u`" \
      --setenv PATH "/usr/bin" \
      --setenv SWI_HOME_DIR "/lib/swipl" \
      --setenv LANG "en_US.UTF-8" \
      --setenv TERM "${TERM}" \
      --setenv TERMINFO "/lib/terminal/terminfo" \
      --setenv TERMINFO_DIRS "/usr/share/terminfo" \
      --file 11 /etc/passwd \
      --file 12 /etc/group \
    swipl -s /lib/PeTTa/src/metta.pl -g "${GOAL}" -t halt \
    11< <(getent passwd $UID 65534) \
    12< <(getent group $(id -g) 65534) \
)
    # swipl \
    # bash \
    # swipl -s /lib/PeTTa/src/main.pl -g "${GOAL}" -t halt \
