# Disgustang
Disgustang is a companion program to EWW and Dunst for the sole purpose of
saving notification icons. This is because Dunst does not in any way save
notification icon data in its history and thus cannot be recalled by `dunstctl
history`, etc.

Disgustang is a nasty hack that creates a DBus monitor session listening for
relevant messages in order to maintain its own history alongside Dunst, but
while saving notification icons to disk and writing them to a JSON array such
that they can be referenced in your EWW config.
