"""A throwaway FTP server for the device test.

Serves one directory with full read/write permissions, because the web app checks upload, delete,
mkdir and rmdir. It exists so the test does not depend on the real NAS being up, having the
expected contents, or being safe to write to.

Configured entirely through the environment so the caller has one way of passing things:

    ALBUM_ROOT    directory to serve        (required)
    FTP_PORT      port to listen on         (default 2121)
    FTP_USER      user name                 (default tester)
    FTP_PASSWORD  password                  (default testerpass)
"""

import os
import sys

from pyftpdlib.authorizers import DummyAuthorizer
from pyftpdlib.handlers import FTPHandler
from pyftpdlib.servers import FTPServer

root = os.environ.get("ALBUM_ROOT")
if not root:
    sys.exit("ALBUM_ROOT is required")
root = os.path.abspath(os.path.expanduser(root))
if not os.path.isdir(root):
    sys.exit(f"ALBUM_ROOT is not a directory: {root}")

port = int(os.environ.get("FTP_PORT", "2121"))
user = os.environ.get("FTP_USER", "tester")
password = os.environ.get("FTP_PASSWORD", "testerpass")

authorizer = DummyAuthorizer()
# elradfmwMT: everything, because the checks exercise upload, delete, mkdir and rmdir
authorizer.add_user(user, password, root, perm="elradfmwMT")

handler = FTPHandler
handler.authorizer = authorizer
# Loopback only. The frame under test runs on this same device, and a test server with full write
# permissions has no business being reachable from the network.
handler.masquerade_address = "127.0.0.1"
handler.passive_ports = range(60000, 60020)

server = FTPServer(("127.0.0.1", port), handler)
print(f"serving {root} on ftp://127.0.0.1:{port} as {user}", flush=True)
server.serve_forever()
