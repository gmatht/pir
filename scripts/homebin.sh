#!/bin/bash
# Install pir into ~/bin
set -e
DEST=~/bin/pir
cargo build --release 
i=1
while [ -e $DEST$i ]
do i=$((i+1))
done
mv $DEST $DEST$i
cp target/release/pir $DEST
ls -lh $DEST
