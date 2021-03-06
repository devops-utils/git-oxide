#!/bin/bash

set -eu -o pipefail

./etc/check-package-size.sh

for crate in git-features git-ref git-object git-odb git-repository git-transport gitoxide-core .; do
  (cd $crate && cargo release "$@")
done
