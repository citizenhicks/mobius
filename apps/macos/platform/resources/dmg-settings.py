# dmgbuild settings for the download: the window every Mac installer opens with, möbius
# on the left and Applications on the right, an arrow between them. `defines['app']` is
# the notarized, stapled bundle.
import os.path
import unicodedata

app = defines["app"]  # noqa: F821 (provided by dmgbuild)
name = os.path.basename(app)

files = [app]
symlinks = {"Applications": "/Applications"}
icon = os.path.join(app, "Contents/Resources/AppIcon.icns")
format = "ULFO"
background = "platform/resources/dmg-background.png"
window_rect = ((200, 160), (600, 360))
default_view = "icon-view"
show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
icon_size = 128
text_size = 13
# Finder matches positions by the name as stored on the volume, and "möbius" can be
# stored composed or decomposed; place both spellings or the app lands top-left.
icon_locations = {
    unicodedata.normalize(form, name): (150, 170) for form in ("NFC", "NFD")
} | {"Applications": (450, 170)}
