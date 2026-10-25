"""Finds the WebKit helper processes that work for an app.

WebKit's helpers (WebContent, Networking, GPU) are launched by launchd, so they are not the app's
children. macOS records which process they work for as their "responsible process" (the call Activity
Monitor uses); `launchctl procinfo` shows it only to root.

  responsible.py owner PID                 prints the responsible pid of PID (exit 2 unless it is a positive number)
  responsible.py webkit OWNER [ROOT_PID]   prints WebKit pids whose responsible pid is OWNER and that started
                                           no earlier than ROOT_PID (second resolution)
"""
import ctypes
import re
import subprocess
import sys
import time

library = ctypes.CDLL("/usr/lib/system/libquarantine.dylib")
library.responsibility_get_pid_responsible_for_pid.argtypes = [ctypes.c_int]
library.responsibility_get_pid_responsible_for_pid.restype = ctypes.c_int


def responsible(pid):
    return library.responsibility_get_pid_responsible_for_pid(int(pid))


def started(pid):
    out = subprocess.run(["ps", "-o", "lstart=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    return time.mktime(time.strptime(re.sub(r"\s+", " ", out), "%a %b %d %H:%M:%S %Y")) if out else None


def main():
    mode = sys.argv[1]
    if mode == "owner":
        owner = responsible(sys.argv[2])
        if owner <= 0:
            sys.exit(2)
        print(owner)
    elif mode == "webkit":
        owner = int(sys.argv[2])
        if owner <= 0:
            sys.exit(2)
        floor = started(sys.argv[3]) if len(sys.argv) > 3 else None
        pids = subprocess.run(["pgrep", "-f", "com.apple.WebKit"], capture_output=True, text=True).stdout.split()
        for pid in pids:
            if responsible(pid) != owner:
                continue
            begun = started(pid)
            if floor is not None and (begun is None or begun < floor):
                continue
            print(pid)
    else:
        sys.exit(2)


main()
