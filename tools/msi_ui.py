#!/usr/bin/env python3
"""msi_ui.py -- give the MSI wixl built an installer UI, which wixl cannot write. Run by tools/build_msi.sh after
wixl:

    python3 tools/msi_ui.py target/symbiotic-visa-<version>-windows-x64.msi

Writes the Dialog, Control, ControlEvent, EventMapping, TextStyle, UIText and ActionText tables, and adds rows to the
Property, CustomAction and InstallUISequence tables wixl wrote (msibuild -i replaces a whole table, so those three
are exported, extended and imported again). The InstallExecuteSequence is not touched: the UI only decides what is
installed, where, and that the user asked for it.

The pages: Welcome -> Custom setup (features and location) -> Ready (Install, with the elevation shield) -> Progress
-> Finished. Installed already: Change, Repair or Remove -> Ready -> Progress -> Finished. Cancel asks first. msiexec
/qn shows none of them.
"""
import os
import subprocess
import sys
import tempfile

PRODUCT = "Viper488 Symbiotic VISA"
W, H = 370, 270  # every page's size, in installer units

# Dialog attributes and control attributes (the MSI SDK's msidbDialogAttributes* and msidbControlAttributes*).
VISIBLE, MODAL, KEEP_MODELESS, ERROR = 1, 2, 16, 65536
PAGE = VISIBLE | MODAL
C_VISIBLE, C_ENABLED, C_SUNKEN, C_INDIRECT = 1, 2, 4, 8
C_TRANSPARENT = 0x10000
C_PROGRESS95, C_FIXED_VOLUME, C_REMOTE_VOLUME = 0x10000, 0x20000, 0x40000
C_ELEVATION_SHIELD = 0x800000
TEXT = C_VISIBLE | C_ENABLED | C_TRANSPARENT
BUTTON = C_VISIBLE | C_ENABLED

dialogs = []   # (Dialog, Attributes, Title, Control_First, Control_Default, Control_Cancel, Width, Height)
controls = []  # (Dialog_, Control, Type, X, Y, Width, Height, Attributes, Property, Text, Control_Next)
events = []    # (Dialog_, Control_, Event, Argument, Condition, Ordering)
mappings = []  # (Dialog_, Control_, Event, Attribute)


def dialog(name, title, first, default, cancel, ctls, attrs=PAGE, w=W, h=H):
    """A dialog and its controls; the tab order is the order of the focusable ones in `ctls`, in a ring."""
    dialogs.append((name, attrs, title, first, default, cancel, w, h))
    tab = [c[0] for c in ctls if c[1] in ("PushButton", "SelectionTree", "PathEdit", "DirectoryCombo", "DirectoryList", "ListBox", "CheckBox")]
    for c in ctls:
        name_c, typ, x, y, cw, ch, a, prop, text = c
        nxt = tab[(tab.index(name_c) + 1) % len(tab)] if name_c in tab and len(tab) > 1 else ""
        controls.append((name, name_c, typ, x, y, cw, ch, a, prop, text, nxt))


def on(dlg, ctl, *evs):
    """The events a control publishes, in order: (event, argument[, condition])."""
    for i, e in enumerate(evs, 1):
        events.append((dlg, ctl, e[0], e[1], e[2] if len(e) > 2 else "1", i))


def title(text, sub=""):
    """A page's heading and the line under it, and the rule below the buttons' row."""
    out = [("Title", "Text", 15, 10, 340, 18, TEXT, "", "{\\SymTitle}" + text)]
    if sub:
        out.append(("Sub", "Text", 15, 30, 340, 24, TEXT, "", sub))
    out.append(("TopLine", "Line", 0, 56, W, 0, C_VISIBLE, "", ""))
    out.append(("BottomLine", "Line", 0, 234, W, 0, C_VISIBLE, "", ""))
    return out


def buttons(back=None, nxt="Next >", cancel="Cancel", nxt_attrs=BUTTON):
    out = []
    if back is not None:
        out.append(("Back", "PushButton", 180, 243, 56, 17, BUTTON if back else C_VISIBLE, "", "< Back"))
    out.append(("Next", "PushButton", 236, 243, 56, 17, nxt_attrs, "", nxt))
    if cancel:
        out.append(("Cancel", "PushButton", 304, 243, 56, 17, BUTTON, "", cancel))
    return out


def mode(m, ready_title, ready_text, go, progress, done):
    """The events that set the wording of the pages after this choice (Ready, Progress, Finished)."""
    return [("[SymMode]", m), ("[SymReadyTitle]", ready_title), ("[SymReadyText]", ready_text), ("[SymGo]", go),
            ("[SymProgressTitle]", progress), ("[SymDoneText]", done)]


INSTALL = mode("Install", "Ready to install",
               f"Click Install to install {PRODUCT}. Windows will ask for administrator permission. Click Back to change the features or the location.",
               "&Install", f"Installing {PRODUCT}", f"{PRODUCT} has been installed.")
CHANGE = mode("Change", "Ready to change",
              f"Click Change to apply the features you chose. Windows will ask for administrator permission.",
              "&Change", f"Changing {PRODUCT}", f"{PRODUCT} has been changed.")
REPAIR = mode("Repair", "Ready to repair",
              f"Click Repair to put back every file, registry entry and registration of {PRODUCT}. Windows will ask for administrator permission.",
              "&Repair", f"Repairing {PRODUCT}", f"{PRODUCT} has been repaired.")
REMOVE = mode("Remove", "Ready to remove",
              f"Click Remove to remove {PRODUCT} from this computer. The settings in the shared configuration folder are kept. Windows will ask for administrator permission.",
              "&Remove", f"Removing {PRODUCT}", f"{PRODUCT} has been removed.")
CANCEL = ("SpawnDialog", "CancelDlg")

# Welcome: nothing is done until Install on the Ready page.
dialog("WelcomeDlg", "[ProductName] Setup", "Next", "Next", "Cancel", title(f"Welcome to {PRODUCT} Setup") + [
    ("Body", "Text", 15, 70, 340, 120, TEXT, "",
     f"Setup installs {PRODUCT} [ProductVersion]: a VISA library (visa64.dll and visa32.dll) for GPIB adapters, USBTMC, "
     "LAN and serial instruments, its background service, Symbiotic Explorer and the command-line tool. "
     "On the next page you choose the features and where to install them. Nothing is changed on this computer "
     "until you click Install on the last page."),
] + buttons(back=False))
on("WelcomeDlg", "Next", *INSTALL, ("NewDialog", "CustomizeDlg"))
on("WelcomeDlg", "Cancel", CANCEL)

# Custom setup: the feature tree, the selected feature's description, size and location.
dialog("CustomizeDlg", "[ProductName] Setup", "Tree", "Next", "Cancel", title(
    "Custom setup", "Choose the features to install, and where. Click a feature's icon to change how it is installed.") + [
    ("Tree", "SelectionTree", 15, 65, 190, 135, C_VISIBLE | C_ENABLED | C_SUNKEN, "_BrowseProperty", "Features"),
    ("ItemDesc", "Text", 215, 65, 140, 95, TEXT, "", "Select a feature to see what it does."),
    ("ItemSize", "Text", 215, 165, 140, 35, TEXT, "", ""),
    ("LocationLabel", "Text", 15, 208, 50, 12, TEXT, "", "Location:"),
    ("Location", "Text", 65, 208, 225, 24, TEXT, "", ""),
    ("Browse", "PushButton", 300, 205, 56, 17, BUTTON, "", "B&rowse..."),
] + buttons(back=True))
mappings += [("CustomizeDlg", "ItemDesc", "SelectionDescription", "Text"),
             ("CustomizeDlg", "ItemSize", "SelectionSize", "Text"),
             ("CustomizeDlg", "Location", "SelectionPath", "Text"),
             ("CustomizeDlg", "Location", "SelectionPathOn", "Visible"),
             ("CustomizeDlg", "LocationLabel", "SelectionPathOn", "Visible"),
             ("CustomizeDlg", "Browse", "SelectionPathOn", "Enabled")]
on("CustomizeDlg", "Browse", ("SelectionBrowse", "BrowseDlg"))
on("CustomizeDlg", "Back", ("NewDialog", "WelcomeDlg", "NOT Installed"), ("NewDialog", "MaintenanceDlg", "Installed"))
on("CustomizeDlg", "Next", ("NewDialog", "VerifyReadyDlg"))
on("CustomizeDlg", "Cancel", CANCEL)

# Browse: another folder for the feature selected (the runtime's is INSTALLDIR).
dialog("BrowseDlg", "[ProductName] Setup", "PathEdit", "OK", "Cancel", title(
    "Change the location", "Browse to the folder to install into.") + [
    ("LookIn", "Text", 15, 66, 45, 12, TEXT, "", "&Look in:"),
    ("Combo", "DirectoryCombo", 60, 63, 230, 80, C_VISIBLE | C_ENABLED | C_INDIRECT | C_FIXED_VOLUME | C_REMOTE_VOLUME, "_BrowseProperty", ""),
    ("Up", "PushButton", 298, 63, 28, 17, BUTTON, "", "Up"),
    ("NewFolder", "PushButton", 328, 63, 28, 17, BUTTON, "", "New"),
    ("List", "DirectoryList", 15, 85, 341, 105, C_VISIBLE | C_ENABLED | C_SUNKEN | C_INDIRECT, "_BrowseProperty", ""),
    ("FolderLabel", "Text", 15, 197, 70, 12, TEXT, "", "&Folder name:"),
    ("PathEdit", "PathEdit", 15, 210, 341, 18, C_VISIBLE | C_ENABLED | C_SUNKEN | C_INDIRECT, "_BrowseProperty", ""),
    ("OK", "PushButton", 236, 243, 56, 17, BUTTON, "", "OK"),
    ("Cancel", "PushButton", 304, 243, 56, 17, BUTTON, "", "Cancel"),
])
on("BrowseDlg", "Up", ("DirectoryListUp", "0"))
on("BrowseDlg", "NewFolder", ("DirectoryListNew", "0"))
on("BrowseDlg", "OK", ("SetTargetPath", "[_BrowseProperty]"), ("EndDialog", "Return"))
on("BrowseDlg", "Cancel", ("Reset", "0"), ("EndDialog", "Return"))

# Installed already (double-clicked again): change, repair or remove.
dialog("MaintenanceDlg", "[ProductName] Setup", "Change", "Change", "Cancel", title(
    f"Change, repair or remove {PRODUCT}", f"{PRODUCT} [ProductVersion] is installed on this computer.") + [
    ("Change", "PushButton", 25, 72, 80, 17, BUTTON, "", "&Change"),
    ("ChangeText", "Text", 115, 72, 240, 30, TEXT, "", "Choose which features are installed."),
    ("Repair", "PushButton", 25, 112, 80, 17, BUTTON, "", "Re&pair"),
    ("RepairText", "Text", 115, 112, 240, 30, TEXT, "", "Put back the installed files, registry entries and registrations."),
    ("Remove", "PushButton", 25, 152, 80, 17, BUTTON, "", "&Remove"),
    ("RemoveText", "Text", 115, 152, 240, 30, TEXT, "", f"Remove {PRODUCT} from this computer. The shared configuration is kept."),
] + buttons(back=False, nxt="Next >", nxt_attrs=C_VISIBLE))
on("MaintenanceDlg", "Change", *CHANGE, ("NewDialog", "CustomizeDlg"))
on("MaintenanceDlg", "Repair", *REPAIR, ("NewDialog", "VerifyReadyDlg"))
on("MaintenanceDlg", "Remove", *REMOVE, ("NewDialog", "VerifyReadyDlg"))
on("MaintenanceDlg", "Cancel", CANCEL)

# Ready: the one button that starts the work (and the UAC prompt).
dialog("VerifyReadyDlg", "[ProductName] Setup", "Next", "Next", "Cancel", title("[SymReadyTitle]") + [
    ("Body", "Text", 15, 70, 340, 80, TEXT, "", "[SymReadyText]"),
] + buttons(back=True, nxt="[SymGo]", nxt_attrs=BUTTON | C_ELEVATION_SHIELD))
on("VerifyReadyDlg", "Back", ("NewDialog", "CustomizeDlg", 'SymMode = "Install" OR SymMode = "Change"'),
   ("NewDialog", "MaintenanceDlg", 'SymMode = "Repair" OR SymMode = "Remove"'))
on("VerifyReadyDlg", "Next", ("Remove", "ALL", 'SymMode = "Remove"'), ("Reinstall", "ALL", 'SymMode = "Repair"'),
   ("ReinstallMode", "amus", 'SymMode = "Repair"'), ("EndDialog", "Return"))
on("VerifyReadyDlg", "Cancel", CANCEL)

# Progress, while the installer works (modeless: the work runs behind it).
dialog("ProgressDlg", "[ProductName] Setup", "Cancel", "Cancel", "Cancel", title("[SymProgressTitle]") + [
    ("Body", "Text", 15, 70, 340, 20, TEXT, "", "Please wait."),
    ("ActionText", "Text", 15, 100, 340, 24, TEXT, "", ""),
    ("Bar", "ProgressBar", 15, 130, 340, 12, C_VISIBLE | C_PROGRESS95, "", "Progress"),
] + buttons(nxt="Next >", nxt_attrs=C_VISIBLE), attrs=VISIBLE)
mappings += [("ProgressDlg", "ActionText", "ActionText", "Text"), ("ProgressDlg", "Bar", "SetProgress", "Progress")]
on("ProgressDlg", "Cancel", CANCEL)

# The three ends: finished, cancelled, failed.
for name, head, text, how in [
    ("ExitDlg", "Finished", "[SymDoneText]", "Return"),
    ("UserExitDlg", "Setup was cancelled", f"{PRODUCT} Setup was cancelled. Nothing on this computer was changed.", "Exit"),
    ("FatalErrorDlg", "Setup failed", f"{PRODUCT} Setup ended because of an error. Nothing on this computer was changed.", "Exit"),
]:
    dialog(name, "[ProductName] Setup", "Finish", "Finish", "Finish", title(head) + [
        ("Body", "Text", 15, 70, 340, 80, TEXT, "", text),
        ("Finish", "PushButton", 304, 243, 56, 17, BUTTON, "", "&Finish"),
    ])
    on(name, "Finish", ("EndDialog", how))

# Cancel, asked from any page.
dialog("CancelDlg", "[ProductName] Setup", "No", "No", "No", [
    ("Text", "Text", 15, 15, 230, 30, TEXT, "", f"Do you want to cancel {PRODUCT} Setup?"),
    ("Yes", "PushButton", 130, 57, 56, 17, BUTTON, "", "&Yes"),
    ("No", "PushButton", 192, 57, 56, 17, BUTTON, "", "&No"),
], w=260, h=85)
on("CancelDlg", "Yes", ("EndDialog", "Exit"))
on("CancelDlg", "No", ("EndDialog", "Return"))

# The installer's own errors (the ErrorDialog property names it; it shows the buttons the error needs).
err_buttons = [("A", "&Abort", "ErrorAbort"), ("C", "&Cancel", "ErrorCancel"), ("I", "&Ignore", "ErrorIgnore"),
               ("N", "&No", "ErrorNo"), ("O", "&OK", "ErrorOk"), ("R", "&Retry", "ErrorRetry"), ("Y", "&Yes", "ErrorYes")]
dialog("ErrorDlg", "[ProductName] Setup", "ErrorText", "O", "C", [
    ("ErrorText", "Text", 15, 15, 240, 60, TEXT, "", "Information text"),
] + [(b, "PushButton", 100, 80, 56, 17, BUTTON, "", t) for b, t, _ in err_buttons], attrs=VISIBLE | MODAL | ERROR, w=270, h=105)
for b, _, e in err_buttons:
    on("ErrorDlg", b, ("EndDialog", e))
# Error dialogs have no tab ring of their own: the installer shows only some of the buttons.
controls[:] = [c[:10] + ("",) if c[0] == "ErrorDlg" else c for c in controls]

# A file in use (Symbiotic Explorer still open, say).
dialog("FilesInUse", "[ProductName] Setup", "Retry", "Retry", "Exit", title(
    "Files in use", "These programs use files that Setup has to change. Close them and click Retry.") + [
    ("List", "ListBox", 15, 65, 340, 130, C_VISIBLE | C_ENABLED | C_SUNKEN, "FileInUseProcess", ""),
    ("Retry", "PushButton", 168, 243, 56, 17, BUTTON, "", "&Retry"),
    ("Ignore", "PushButton", 236, 243, 56, 17, BUTTON, "", "&Ignore"),
    ("Exit", "PushButton", 304, 243, 56, 17, BUTTON, "", "E&xit"),
], attrs=PAGE | KEEP_MODELESS)
on("FilesInUse", "Retry", ("EndDialog", "Retry"))
on("FilesInUse", "Ignore", ("EndDialog", "Ignore"))
on("FilesInUse", "Exit", ("EndDialog", "Exit"))

FEATURE_ORDER = ["Runtime", "UsbtmcWinUsb", "RegisterVisa", "RegisterVisaNet", "RegisterVisaCom"]

text_styles = [("SymTitle", "Segoe UI", 12, None, 1), ("SymNormal", "Segoe UI", 9, None, 0)]

properties = [
    ("DefaultUIFont", "SymNormal"),
    ("ErrorDialog", "ErrorDlg"),
    ("SymMode", "Install"),
    ("SymProgressTitle", f"Installing {PRODUCT}"),
    ("SymDoneText", f"{PRODUCT} has been installed."),
]

# A removal started outside the pages (Settings' Uninstall: msiexec /x) gets the right wording too.
custom_actions = [
    ("SymRemovingTitle", 51, "SymProgressTitle", f"Removing {PRODUCT}"),
    ("SymRemovedText", 51, "SymDoneText", f"{PRODUCT} has been removed."),
]

ui_sequence = [
    ("SetInstallDir", "PREVIOUSINSTALLDIR AND NOT INSTALLDIR", 990),  # the installed version's location (symbiotic.wxs)
    ("SymRemovingTitle", 'REMOVE = "ALL"', 1240),
    ("SymRemovedText", 'REMOVE = "ALL"', 1241),
    ("WelcomeDlg", "NOT Installed", 1250),
    ("MaintenanceDlg", "Installed AND NOT RESUME AND NOT Preselected", 1251),
    ("ProgressDlg", "", 1280),
    ("ExitDlg", "", -1),
    ("UserExitDlg", "", -2),
    ("FatalErrorDlg", "", -3),
]

# The SelectionTree's and the cost texts, which the installer looks up by these keys.
ui_text = [
    ("AbsentPath", ""), ("bytes", "bytes"), ("GB", "GB"), ("KB", "KB"), ("MB", "MB"),
    ("MenuAbsent", "Entire feature will be unavailable"),
    ("MenuAdvertise", "Feature will be installed when required"),
    ("MenuAllLocal", "Entire feature will be installed on local hard drive"),
    ("MenuLocal", "Will be installed on local hard drive"),
    ("NewFolder", "Folder|New Folder"),
    ("SelAbsentAbsent", "This feature will remain uninstalled."),
    ("SelAbsentLocal", "This feature will be installed on the local hard drive."),
    ("SelLocalAbsent", "This feature will be removed from your local hard drive."),
    ("SelLocalLocal", "This feature will remain on your local hard drive."),
    ("SelChildCostNeg", "This feature frees up [1] on your hard drive."),
    ("SelChildCostPos", "This feature requires [1] on your hard drive."),
    ("SelCostPending", "Compiling cost for this feature..."),
    ("SelParentCostNegNeg", "This feature frees up [1] on your hard drive. It has [2] of [3] subfeatures selected. The subfeatures free up [4] on your hard drive."),
    ("SelParentCostNegPos", "This feature frees up [1] on your hard drive. It has [2] of [3] subfeatures selected. The subfeatures require [4] on your hard drive."),
    ("SelParentCostPosNeg", "This feature requires [1] on your hard drive. It has [2] of [3] subfeatures selected. The subfeatures free up [4] on your hard drive."),
    ("SelParentCostPosPos", "This feature requires [1] on your hard drive. It has [2] of [3] subfeatures selected. The subfeatures require [4] on your hard drive."),
    ("TimeRemaining", "Time remaining: {[1] minutes }{[2] seconds}"),
    ("VolumeCostAvailable", "Available"), ("VolumeCostDifference", "Difference"), ("VolumeCostRequired", "Required"),
    ("VolumeCostSize", "Disk Size"), ("VolumeCostVolume", "Volume"),
    ("ScriptInProgress", "Gathering required information..."),
]

# What the progress page says for each step (ActionText control).
action_text = [
    ("InstallValidate", "Validating install", ""),
    ("InstallFiles", "Copying new files", "File: [1], Directory: [9], Size: [6]"),
    ("RemoveFiles", "Removing files", "File: [1], Directory: [9]"),
    ("RemoveExistingProducts", "Removing the previous version", ""),
    ("WriteRegistryValues", "Writing system registry values", "Key: [1], Name: [2]"),
    ("RemoveRegistryValues", "Removing system registry values", "Key: [1], Name: [2]"),
    ("WriteEnvironmentStrings", "Updating environment strings", "Name: [1], Value: [2]"),
    ("RemoveEnvironmentStrings", "Updating environment strings", "Name: [1], Value: [2]"),
    ("CreateShortcuts", "Creating shortcuts", "Shortcut: [1]"),
    ("RemoveShortcuts", "Removing shortcuts", "Shortcut: [1]"),
    ("CreateFolders", "Creating folders", "Folder: [1]"),
    ("RemoveFolders", "Removing folders", "Folder: [1]"),
    ("StopDaemons", "Stopping the Symbiotic service", ""),
    ("StopDaemonsQuietly", "Stopping the Symbiotic service", ""),
    ("OpenData", "Opening the shared configuration folder to all users", ""),
    ("RegisterIvi", "Registering with the IVI VISA router", ""),
    ("UnregisterIvi", "Unregistering from the IVI VISA router", ""),
    ("RegisterNet", "Registering the VISA.NET implementation", ""),
    ("UnregisterNet", "Unregistering the VISA.NET implementation", ""),
    ("RegisterCom", "Registering the VISA COM implementation", ""),
    ("UnregisterCom", "Unregistering the VISA COM implementation", ""),
    ("InstallUsbtmc", "Installing the WinUSB driver package for USBTMC instruments", ""),
    ("UninstallUsbtmc", "Removing the WinUSB driver package for USBTMC instruments", ""),
    ("InstallWebView2", "Installing Microsoft WebView2", ""),
]


def cell(v):
    if v is None:
        return ""
    s = str(v)
    if any(ch in s for ch in "\t\r\n"):
        raise ValueError(f"a tab or line break in {s!r}")
    return s


def idt(path, header, types, table_keys, rows):
    with open(path, "w", encoding="ascii", newline="\r\n") as f:
        f.write("\t".join(header) + "\n" + "\t".join(types) + "\n" + "\t".join(table_keys) + "\n")
        for r in rows:
            f.write("\t".join(cell(v) for v in r) + "\n")


def extend(msi, table, tmp, rows):
    """Export `table`, add `rows` (replacing rows of the same first column), import it again."""
    path = os.path.join(tmp, table + ".idt")
    text = subprocess.run(["msiinfo", "export", msi, table], check=True, capture_output=True, text=True).stdout
    lines = text.splitlines()
    head, body = lines[:3], lines[3:]
    ours = {cell(r[0]) for r in rows}
    body = [l for l in body if l.split("\t")[0] not in ours]
    width = len(head[0].split("\t"))
    rows = [tuple(r) + ("",) * (width - len(r)) for r in rows]
    with open(path, "w", encoding="utf-8", newline="\r\n") as f:
        f.write("\n".join(head + body + ["\t".join(cell(v) for v in r) for r in rows]) + "\n")
    return path


def main():
    msi = sys.argv[1]
    with tempfile.TemporaryDirectory() as tmp:
        p = lambda n: os.path.join(tmp, n + ".idt")
        idt(p("Dialog"), ["Dialog", "HCentering", "VCentering", "Width", "Height", "Attributes", "Title", "Control_First", "Control_Default", "Control_Cancel"],
            ["s72", "i2", "i2", "i2", "i2", "I4", "L128", "s50", "S50", "S50"], ["Dialog", "Dialog"],
            [(d, 50, 50, w, h, a, t, f, dft, c) for d, a, t, f, dft, c, w, h in dialogs])
        idt(p("Control"), ["Dialog_", "Control", "Type", "X", "Y", "Width", "Height", "Attributes", "Property", "Text", "Control_Next", "Help"],
            ["s72", "s50", "s20", "i2", "i2", "i2", "i2", "I4", "S72", "L0", "S50", "L50"], ["Control", "Dialog_", "Control"],
            [c + ("",) for c in controls])
        idt(p("ControlEvent"), ["Dialog_", "Control_", "Event", "Argument", "Condition", "Ordering"],
            ["s72", "s50", "s50", "s255", "S255", "I2"], ["ControlEvent", "Dialog_", "Control_", "Event", "Argument", "Condition"], events)
        idt(p("EventMapping"), ["Dialog_", "Control_", "Event", "Attribute"], ["s72", "s50", "s50", "s50"],
            ["EventMapping", "Dialog_", "Control_", "Event"], mappings)
        idt(p("TextStyle"), ["TextStyle", "FaceName", "Size", "Color", "StyleBits"], ["s72", "s32", "i2", "I4", "I2"],
            ["TextStyle", "TextStyle"], text_styles)
        idt(p("UIText"), ["Key", "Text"], ["s72", "L255"], ["UIText", "Key"], ui_text)
        idt(p("ActionText"), ["Action", "Description", "Template"], ["s72", "L0", "L0"], ["ActionText", "Action"], action_text)
        tables = [p(n) for n in ("Dialog", "Control", "ControlEvent", "EventMapping", "TextStyle", "UIText", "ActionText")]
        # The location chosen on the pages goes to the elevated part of the install.
        prop = subprocess.run(["msiinfo", "export", msi, "Property"], check=True, capture_output=True, text=True).stdout
        secure = next((l.split("\t")[1] for l in prop.splitlines()[3:] if l.startswith("SecureCustomProperties\t")), "")
        secure = ";".join([x for x in secure.split(";") if x and x != "INSTALLDIR"] + ["INSTALLDIR"])
        tables.append(extend(msi, "Property", tmp, properties + [("SecureCustomProperties", secure)]))
        tables.append(extend(msi, "CustomAction", tmp, custom_actions))
        tables.append(extend(msi, "InstallUISequence", tmp, ui_sequence))
        # The features in the tree in this order (Display), the runtime first.
        feat = subprocess.run(["msiinfo", "export", msi, "Feature"], check=True, capture_output=True, text=True).stdout.splitlines()
        rows = [l.split("\t") for l in feat[3:]]
        for r in rows:
            r[4] = str(FEATURE_ORDER.index(r[0]) + 1)
        tables.append(extend(msi, "Feature", tmp, rows))
        subprocess.run(["msibuild", msi, "-i", *tables], check=True)


main()
