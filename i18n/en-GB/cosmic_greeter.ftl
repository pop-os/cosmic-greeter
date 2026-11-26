cancel = Cancel
suspend = Suspend
restart = Restart
shutdown = Shut down
on-screen-keyboard = On-screen keyboard
accessibility = Accessibility
    .screen-reader = Screen Reader
    .magnifier = Magnifier
    .high-contrast = High contrast
    .invert-colors = Invert Colours
authenticating = Authenticating...
caps-lock = Caps Lock is active.
enter-user = Enter name manually...
type-username = Username:
keyboard-layout = Keyboard layout
restart-now = Restart now?
session = Session
shutdown-now = Shut down now?
user = User
auth-error-default = Authentication failed. Please try again.
auth-error-credentials = Incorrect password. Please check your keyboard layout and try again.
auth-error-denied = Access denied.
auth-error-maxtries = Too many failed authentication attempts.
auth-error-account = Account is unavailable or disabled.
restart-timeout =
    The system will restart automatically
    { $seconds ->
        [0] now.
        [1] in 1 second.
       *[other] in { $seconds } seconds.
    }
shutdown-timeout =
    The system will shut down automatically
    { $seconds ->
        [0] now.
        [1] in 1 second.
       *[other] in { $seconds } seconds.
    }
