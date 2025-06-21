#!/bin/bash

set -e

cat << EOF
Package: tunnelbana
Version: $1
Maintainer: valkyrie_pilot <valk@randomairborne.dev>
Depends: libc6
Architecture: $2
Homepage: https://github.com/randomairborne/tunnelbana
Description: http endpoint server
EOF
