#!/bin/sh
rm -rf /run/cosmic-greeter/cosmic/com.system76.CosmicSettingsDaemon/v1/*
exec cosmic-comp systemd-cat -t cosmic-greeter cosmic-greeter
