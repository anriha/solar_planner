# Deye solar bridge - build / deploy / probe recipes (plan.md Task 0.2).
# Pi coordinates live here, not scattered through recipes or the code.

pi_host := "konazumi@100.64.0.9"
remote_dir := "solar-bridge"
remote_bin := remote_dir + "/solar_controller"
ha_host := "hassio@192.168.1.187"

default:
    @just --list

test:
    cargo test

build:
    nix build .#default

# aarch64 musl static binary for the Pi
build-arm:
    nix build .#solar-bridge-arm

deploy: build-arm
    ssh {{ pi_host }} "mkdir -p {{ remote_dir }}"
    scp result/bin/solar_controller {{ pi_host }}:{{ remote_bin }}.new
    ssh {{ pi_host }} "mv {{ remote_bin }}.new {{ remote_bin }} && systemctl --user restart solar-bridge"

probe *args:
    ssh {{ pi_host }} "{{ remote_bin }} probe {{ args }}"

dump:
    ssh {{ pi_host }} "{{ remote_bin }} dump"

install-unit:
    ssh {{ pi_host }} "mkdir -p ~/.config/systemd/user"
    scp systemd/solar-bridge.service {{ pi_host }}:.config/systemd/user/solar-bridge.service
    ssh {{ pi_host }} "systemctl --user daemon-reload && systemctl --user enable --now solar-bridge"
    @echo "if not done yet: ssh -t {{ pi_host }} 'sudo loginctl enable-linger konazumi'"

deploy-ha:
    ssh {{ ha_host }} "rm -rf /tmp/solar_bridge"
    rsync -Pa custom_components/solar_bridge {{ ha_host }}:/tmp/solar_bridge
    ssh {{ ha_host }} "rm -rf /config/custom_components/solar_bridge && mv /tmp/solar_bridge /config/custom_components/solar_bridge"
    @echo "restart HA (Settings -> System -> Restart) to load the new code"
