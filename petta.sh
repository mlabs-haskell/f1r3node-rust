#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

### BEFORE RUNNING ###
# 
# Make sure the following programs are available in the PATH:
# * swipl
# * python3
# Make sure the following variables are defined:
# * PETTA_DIR: path to PeTTa (the top folder in the repository)
# * SANDBOX_LIB_PATH: path to the libsandbox.so library.
# * CACHE_DIR: this location is where patched libraries will be placed in the
#   host system before binding them inside the bubblewrap sandbox. 
# 
# The flake.nix under the nix/ directory provides a development shell with all
# the needed dependencies.
# 
if [[ ! -v PETTA_DIR ]]; then
  echo "The PETTA_DIR variable is not defined. Please set it to the PeTTa \
directory."
  exit 1
fi

if [[ ! -v CACHE_DIR ]]; then
  echo "The CACHE_DIR variable is not defined. Please set it to a directory."
  exit 1
fi

if [[ ! SANDBOX_LIB_PATH ]]; then
  echo "The SANDBOX_LIB_PATH variable is not defined. Please set it to the path\
 of the libsandbox.so library. See https://github.com/cloudflare/sandbox."
  exit 1
fi

# Operating mode: NORMAL (default) or NODE
# NORMAL  — print MeTTa println!/trace! output directly to stdout, emit a single {results:[...]} JSON envelope
# NODE    — emit NDJSON frames: {"channel":"...","arguments":[...]} for each println!/trace!,
#           then a final {"type":"result","value":[...]} line
PETTA_MODE=${PETTA_MODE:-NORMAL}

source "${SCRIPT_DIR}/functions.sh"

### BUBBLEWRAP OPTIONS ###
# Here we calculate the --ro-bind, --dir and --symlink options required to
# create a Prolog compatible filesystem inside the bublewrap sandbox.
#
# Specifically, we need:
# * The Prolog executable and all supporting libraries
# * The PeTTa project directory, which contains the PeTTa interpreter itself as
#   well as other MeTTa libraries provided in it.
# * The libsandbox.so library, which lets us conveniently apply seccomp filters.

# Programs to run in the sandbox
PROGRAMS="swipl bash ls python3"

if [ ! -f ${CACHE_DIR}/cached_programs_binds ]; then
    PROGRAMS_BINDS=$(generate-binds-exes ${PROGRAMS})
else
    PROGRAMS_BINDS=$(<${CACHE_DIR}/cached_programs_binds)
    echo "${PROGRAMS_BINDS}" >${CACHE_DIR}/cached_programs_binds
fi

# Options for adding the python3 /lib directory
PYTHONHOME=$(python3 -c "import sys; print(' '.join(sys.path).strip())" | sed -rn 's/[ ]*([^ ]+)\/lib\/[^ ]*/\1\n/pg' | uniq)
PYTHON_HOME_BINDS="--ro-bind ${PYTHONHOME} /lib/python"

# Options for re-creating the swipl home inside the  sandbox. We can't just
# bind the whole directory because we need to create the symlink
# `/lib/swipl/lib/x86_64-linux -> /lib`.
SWIPL_HOME_DIR=$(swipl --home)
if [ ! -f ${CACHE_DIR}/cached_swipl_home_binds ]; then
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
  echo "${SWIPL_HOME_BINDS}" >${CACHE_DIR}/cached_swipl_home_binds
else
  SWIPL_HOME_BINDS=$(<${CACHE_DIR}/cached_swipl_home_binds)
fi

# Shared libraries that come with the SWI-Prolog distribution. We ignore
# libjpl because it is not needed and it has cyclic dependencies, which halts
# dependency discovery code.
if [ ! -f ${CACHE_DIR}/cached_swipl_libs_binds ]; then
    SWIPL_LIBS=$(find ${SWIPL_HOME_DIR}/lib/x86_64-linux -type f | grep -v libjpl)
    SWIPL_LIBS_BINDS=$(generate-binds-libs ${SWIPL_LIBS})
    echo "${SWIPL_LIBS_BINDS}" >${CACHE_DIR}/cached_swipl_libs_binds
else
    SWIPL_LIBS_BINDS=$(<${CACHE_DIR}/cached_swipl_libs_binds)
fi

# Metta program to run
PROGRAM_FILE=$1
# Session path (simply the CWD of swipl)
SESSION_PATH="/tmp/session"

if [ "${PETTA_MODE}" = "NODE" ]; then
  # NODE mode: emit NDJSON frames for println!/trace!, final result as {"type":"result","value":[...]}
  # Override println! by making it dynamic, abolishing the old clause, and asserting the new frame-emitting clause.
  NODE_BINDS=""
  GOAL="assertz(silent(true)), assertz(working_dir('${SESSION_PATH}')), use_module(library(json)), dynamic('println!'/2), abolish('println!'/2), asserta(('println!'(Arg,true) :- swrite(Arg,RArg), json_write_dict(current_output, _{channel:'rho:io:stdout',arguments:[RArg]}), nl(current_output))), load_metta_file('program.metta', Results), json_write_dict(current_output, _{type:'result', value:Results}), nl(current_output)."
else
  # NORMAL mode (default): emit single {results:[...]} JSON envelope, raw println!/trace! to stdout
  NODE_BINDS=""
  GOAL="assertz(silent(true)), assertz(working_dir('${SESSION_PATH}')), load_metta_file('program.metta', Results), use_module(library(json)), json_write_dict(current_output, #{results:Results})."
fi

### SECCOMP FILTERS ###

if [ ! $SANDBOX_LIB_PATH ]; then
  echo "SANDBOX_LIB_PATH is not defined. Please set it to the path of the
  libsandbox.so library"
  exit 1
else
  if [ ! -f ${CACHE_DIR}/cached_sandbox_lib_binds ]; then
    SANDBOX_LIB_BINDS=$(generate-binds-libs ${SANDBOX_LIB_PATH})
    echo "${SANDBOX_LIB_BINDS}" >${CACHE_DIR}/cached_sandbox_lib_binds
  else
    SANDBOX_LIB_BINDS=$(<${CACHE_DIR}/cached_sandbox_lib_binds)
  fi
fi

# Taken from an audit of syscalls while running the entire PeTTa test suite
SECCOMP_SYSCALL_ALLOW="read:write:open:lseek:mprotect:munmap:brk:rt_sigaction:rt_sigprocmask:access:madvise:getpid:exit:fcntl:getcwd:readlink:sigaltstack:prctl:futex:sched_getaffinity:getdents64:clock_gettime:exit_group:set_robust_list:prlimit64:getrandom:rseq:clone3:openat:fstat:newfstatat:mmap:close:ioctl:rt_sigreturn:mkdir:getuid:getgid:geteuid:getegid:gettid:tgkill:socket:connect"

### BUBBLEWRAP ###

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
      ${PROGRAMS_BINDS} \
      ${SWIPL_LIBS_BINDS} \
      ${SANDBOX_LIB_BINDS} \
      ${PYTHON_HOME_BINDS} \
      ${NODE_BINDS} \
      --symlink ../tmp var/tmp \
      --symlink /lib/PeTTa/lib /tmp/lib \
      --symlink /lib/PeTTa/lib /tmp/session/lib \
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
      --setenv LD_PRELOAD "/lib/libsandbox.so" \
      --setenv SECCOMP_SYSCALL_ALLOW "${SECCOMP_SYSCALL_ALLOW}" \
      --setenv PYTHONHOME "/lib/python" \
      --file 11 /etc/passwd \
      --file 12 /etc/group \
    swipl --stack_limit=8g -q -s /lib/PeTTa/src/metta.pl -g "${GOAL}" -t halt \
    11< <(getent passwd $UID 65534) \
    12< <(getent group $(id -g) 65534) \
)
