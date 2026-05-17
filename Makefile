.PHONY: help setup deploy launch close ssh logs clean serve autostart autostart-clear

APP_ID ?= com.mgl.fourier
SERVE_PORT ?= 8000

help:
	@echo "Targets:"
	@echo "  setup            Create unencrypted SSH key (one-time)"
	@echo "  deploy           Rsync app/ to TV and relaunch"
	@echo "  launch           Launch app on TV"
	@echo "  close            Close app on TV"
	@echo "  ssh              Open SSH shell on TV"
	@echo "  logs             Tail TV system log"
	@echo "  clean            Remove app from TV"
	@echo "  serve            Serve app/ on http://localhost:$(SERVE_PORT) for laptop testing"
	@echo "  autostart        Register app as TV input; reboot TV, then pick it once in Input Hub"
	@echo "  autostart-clear  Unregister app as TV input"

serve:
	@PORT=$(SERVE_PORT) ./scripts/dev-serve

setup:
	@./scripts/setup-key

deploy:
	@./scripts/deploy

launch:
	@./scripts/tv-ssh "/usr/bin/luna-send-pub -n 1 'luna://com.webos.applicationManager/launch' '{\"id\":\"$(APP_ID)\"}'"

close:
	@./scripts/tv-ssh "/usr/bin/luna-send-pub -n 1 'luna://com.webos.applicationManager/dev/closeByAppId' '{\"id\":\"$(APP_ID)\"}'"

ssh:
	@./scripts/tv-ssh

logs:
	@./scripts/tv-ssh "if [ -f /var/log/messages ]; then tail -f /var/log/messages; else journalctl -f -t WebAppMgr -t com.mgl.fourier 2>/dev/null || journalctl -f --no-pager; fi"

clean:
	@./scripts/tv-ssh "rm -rf /media/developer/apps/usr/palm/applications/$(APP_ID)"

autostart:
	@./scripts/tv-ssh "/usr/bin/luna-send-pub -n 1 'luna://com.webos.service.eim/addDevice' '{\"appId\":\"$(APP_ID)\",\"pigImage\":\"\",\"mvpdIcon\":\"\",\"type\":\"MVPD_IP\",\"showPopup\":false,\"label\":\"Fourier\"}'"

autostart-clear:
	@./scripts/tv-ssh "/usr/bin/luna-send-pub -n 1 'luna://com.webos.service.eim/deleteDevice' '{\"appId\":\"$(APP_ID)\"}'"
