#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Self-contained runner for the Python demo client:
# creates a virtualenv, installs dependencies, generates the gRPC stubs from
# ../../proto (the single source of truth), and runs the client.
#
#   ./run.sh <workbook-file> [client.py options]

set -euo pipefail
cd "$(dirname "$0")"

if [ ! -d .venv ]; then
    python3 -m venv .venv
    .venv/bin/pip install --quiet --upgrade pip
    .venv/bin/pip install --quiet -r requirements.txt
fi

# Regenerate stubs whenever the protos are newer than the generated code.
mkdir -p gen
touch gen/__init__.py
if [ ! -f gen/calamine/v1/calamine_service_pb2.py ] \
   || [ ../../proto/calamine/v1/types.proto -nt gen/calamine/v1/types_pb2.py ] \
   || [ ../../proto/calamine/v1/calamine_service.proto -nt gen/calamine/v1/calamine_service_pb2.py ]; then
    .venv/bin/python -m grpc_tools.protoc \
        -I ../../proto \
        --python_out=gen --grpc_python_out=gen --pyi_out=gen \
        calamine/v1/types.proto calamine/v1/calamine_service.proto
    touch gen/calamine/__init__.py gen/calamine/v1/__init__.py
fi

exec .venv/bin/python client.py "$@"
