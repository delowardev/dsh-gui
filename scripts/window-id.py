#!/usr/bin/env python3
"""List on-screen windows via CoreGraphics, so a window can be captured with
`screencapture -l <id>` even when it is buried behind other apps.

Activating the app needs Accessibility permission, which this environment does
not have; reading the window list does not.
"""
import ctypes
import ctypes.util
import sys

cg = ctypes.CDLL(ctypes.util.find_library("CoreGraphics"))
cf = ctypes.CDLL(ctypes.util.find_library("CoreFoundation"))


class CGRect(ctypes.Structure):
    _fields_ = [
        ("x", ctypes.c_double),
        ("y", ctypes.c_double),
        ("w", ctypes.c_double),
        ("h", ctypes.c_double),
    ]


cf.CFStringCreateWithCString.restype = ctypes.c_void_p
cf.CFStringCreateWithCString.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_uint32]
cf.CFDictionaryGetValue.restype = ctypes.c_void_p
cf.CFDictionaryGetValue.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
cf.CFNumberGetValue.restype = ctypes.c_bool
cf.CFNumberGetValue.argtypes = [ctypes.c_void_p, ctypes.c_long, ctypes.c_void_p]
cf.CFStringGetCString.restype = ctypes.c_bool
cf.CFStringGetCString.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_long, ctypes.c_uint32]
cf.CFArrayGetCount.restype = ctypes.c_long
cf.CFArrayGetCount.argtypes = [ctypes.c_void_p]
cf.CFArrayGetValueAtIndex.restype = ctypes.c_void_p
cf.CFArrayGetValueAtIndex.argtypes = [ctypes.c_void_p, ctypes.c_long]

cg.CGWindowListCopyWindowInfo.restype = ctypes.c_void_p
cg.CGWindowListCopyWindowInfo.argtypes = [ctypes.c_uint32, ctypes.c_uint32]
cg.CGRectMakeWithDictionaryRepresentation.restype = ctypes.c_bool
cg.CGRectMakeWithDictionaryRepresentation.argtypes = [ctypes.c_void_p, ctypes.POINTER(CGRect)]

UTF8 = 0x08000100


def key(name):
    return cf.CFStringCreateWithCString(None, name.encode(), UTF8)


def as_str(value):
    if not value:
        return None
    buf = ctypes.create_string_buffer(1024)
    if cf.CFStringGetCString(value, buf, 1024, UTF8):
        return buf.value.decode("utf-8", "replace")
    return None


def as_num(value):
    if not value:
        return None
    out = ctypes.c_double()
    cf.CFNumberGetValue(value, 13, ctypes.byref(out))  # kCFNumberDoubleType
    return out.value


KEYS = {
    name: key(name)
    for name in ("kCGWindowNumber", "kCGWindowOwnerName", "kCGWindowName",
                 "kCGWindowBounds", "kCGWindowLayer")
}

# kCGWindowListOptionAll: a window can be on screen yet fully covered.
windows = cg.CGWindowListCopyWindowInfo(0, 0)
count = cf.CFArrayGetCount(windows)
needle = (sys.argv[1] if len(sys.argv) > 1 else "").lower()

for i in range(count):
    entry = cf.CFArrayGetValueAtIndex(windows, i)
    owner = as_str(cf.CFDictionaryGetValue(entry, KEYS["kCGWindowOwnerName"])) or ""
    if needle and needle not in owner.lower():
        continue
    number = as_num(cf.CFDictionaryGetValue(entry, KEYS["kCGWindowNumber"]))
    layer = as_num(cf.CFDictionaryGetValue(entry, KEYS["kCGWindowLayer"]))
    title = as_str(cf.CFDictionaryGetValue(entry, KEYS["kCGWindowName"])) or ""
    rect = CGRect()
    cg.CGRectMakeWithDictionaryRepresentation(
        cf.CFDictionaryGetValue(entry, KEYS["kCGWindowBounds"]), ctypes.byref(rect)
    )
    print(f"{int(number)}\towner={owner!r}\tlayer={int(layer)}\t"
          f"title={title!r}\t{int(rect.w)}x{int(rect.h)}+{int(rect.x)}+{int(rect.y)}")
