"""Renders the interface in QtWebEngine and saves PNGs.

Usage (Linux): xvfb-run -a python3 tools/screenshot.py PORT TOKEN [OUT_DIR]
Needs python3-pyqt6.qtwebengine and xvfb; works as root in a sandbox.
"""
import json, os, sys
os.environ["QTWEBENGINE_DISABLE_SANDBOX"] = "1"
os.environ["QTWEBENGINE_CHROMIUM_FLAGS"] = "--no-sandbox --disable-gpu --force-device-scale-factor=1"
from PyQt6.QtCore import QTimer, QUrl
from PyQt6.QtWidgets import QApplication
from PyQt6.QtWebEngineWidgets import QWebEngineView
port, token = sys.argv[1], sys.argv[2]
out = sys.argv[3] if len(sys.argv) > 3 else "/tmp"
shots = [("estado", "#/", 1360, 880), ("fotos", "#/ficheiros/Fotos/2026%20F%C3%A9rias%20Algarve", 1360, 880), ("raiz", "#/ficheiros", 1360, 880),
         ("atividade", "#/atividade", 1360, 880), ("definicoes", "#/definicoes", 1360, 1100), ("mobile", "#/ficheiros/Fotos/2026%20F%C3%A9rias%20Algarve", 400, 880)]
app = QApplication(sys.argv)
v = QWebEngineView(); v.resize(1360, 880); v.show()
state = {"i": 0, "started": False}
def shoot():
    i = state["i"]
    if i >= len(shots):
        app.quit(); return
    name, h, w, ht = shots[i]
    v.resize(w, ht)
    v.page().runJavaScript("location.hash=" + json.dumps(h))
    def grab():
        v.grab().save(f"{out}/{name}.png"); state["i"] += 1; shoot()
    QTimer.singleShot(2600, grab)
def loaded(ok):
    if not state["started"]:
        state["started"] = True; QTimer.singleShot(1800, shoot)
v.loadFinished.connect(loaded)
v.load(QUrl(f"http://127.0.0.1:{port}/#t={token}"))
QTimer.singleShot(90000, app.quit)
app.exec()
