#!/bin/bash

set -e

rm -rf target/deb
VERSION=`./ci/get-version.sh`
ARCHNAME=$1

WORKDIR=target/deb/tunnelbana_$VERSION-1_$ARCHNAME
mkdir -p $WORKDIR
mkdir $WORKDIR/DEBIAN
./ci/gencontrol.sh $VERSION $ARCHNAME > $WORKDIR/DEBIAN/control
mkdir -p $WORKDIR/usr/bin $WORKDIR/usr/share/tunnelbana
cp target/release/tunnelbana $WORKDIR/usr/bin
cp -r assets $WORKDIR/usr/share/tunnelbana