"""Prints "<pid> <responsible pid>" for each pid argument.

WebKit's helper processes (WebContent, Networking, GPU) are launched by launchd, so they are not
Photon's children. macOS records which process they work for as their "responsible process"
(the call Activity Monitor uses); `launchctl procinfo` shows it only to root.
"""
import ctypes
import sys

library = ctypes.CDLL("/usr/lib/system/libquarantine.dylib")
library.responsibility_get_pid_responsible_for_pid.argtypes = [ctypes.c_int]
library.responsibility_get_pid_responsible_for_pid.restype = ctypes.c_int
for pid in sys.argv[1:]:
    print(pid, library.responsibility_get_pid_responsible_for_pid(int(pid)))
