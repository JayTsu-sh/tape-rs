"""IBM reference XML parser only; no device access. Runs each fixture in a process."""
import ctypes
import sys
lib = ctypes.CDLL('/opt/ltfs-ref/lib/libltfs.so')
ptr = ctypes.c_void_p
lib.ltfs_init.argtypes = [ctypes.c_int, ctypes.c_bool, ctypes.c_bool]
lib.ltfs_volume_alloc.argtypes = [ctypes.c_char_p, ctypes.POINTER(ptr)]
lib.ltfs_index_alloc.argtypes = [ctypes.POINTER(ptr), ptr]
lib.xml_schema_from_file.argtypes = [ctypes.c_char_p, ptr, ptr]
assert lib.ltfs_init(0, False, False) == 0
volume, index = ptr(), ptr()
assert lib.ltfs_volume_alloc(b'xattr-oracle', ctypes.byref(volume)) == 0
assert lib.ltfs_index_alloc(ctypes.byref(index), volume) == 0
rc = lib.xml_schema_from_file(sys.argv[1].encode(), index, volume)
print('xml_schema_from_file', rc)
assert rc == int(sys.argv[2])
