# TollGate packaging.
#
# Builds installable packages for the platforms a node actually runs on.
# Everything lands in deploy/ at the project root.
#
# Usage:
#   make ipk      OpenWrt .ipk (aarch64 by default; ARCH=x86_64 and so on)
#   make pkg      macOS .pkg installer
#   make all      both
#   make clean    remove deploy/

SHELL := /bin/bash
PACKAGING_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
PROJECT_ROOT  := $(abspath $(PACKAGING_DIR)/..)
ARCH ?= aarch64

.PHONY: all ipk pkg clean

all: ipk pkg

ipk:
	@bash $(PACKAGING_DIR)/openwrt-ipk/build-ipk.sh --arch $(ARCH)

pkg:
	@bash $(PACKAGING_DIR)/macos/build-pkg.sh

clean:
	rm -rf $(PROJECT_ROOT)/deploy
