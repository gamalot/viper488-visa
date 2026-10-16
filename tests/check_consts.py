"""check_consts.py -- every VISA constant in crates/symbiotic-visa/src/consts.rs must be one
pyvisa knows, under the name we think it has.  python check_consts.py [consts.rs]"""
import re, sys
from pyvisa import constants as c

# our name -> pyvisa's member name (StatusCode or ResourceAttribute)
NAMES = {
 "VI_SUCCESS": "success", "VI_SUCCESS_EVENT_DIS": "success_event_already_disabled", "VI_SUCCESS_TERM_CHAR": "success_termination_character_read",
 "VI_SUCCESS_MAX_CNT": "success_max_count_read", "VI_SUCCESS_NESTED_SHARED": "success_nested_shared", "VI_SUCCESS_NESTED_EXCLUSIVE": "success_nested_exclusive",
 "VI_ERROR_RSRC_LOCKED": "error_resource_locked", "VI_ERROR_INV_LOCK_TYPE": "error_invalid_lock_type",
 "VI_ERROR_INV_ACCESS_KEY": "error_invalid_access_key", "VI_ERROR_SESN_NLOCKED": "error_session_not_locked", "VI_ERROR_SYSTEM_ERROR": "error_system_error",
 "VI_ERROR_INV_OBJECT": "error_invalid_object", "VI_ERROR_INV_EXPR": "error_invalid_expression",
 "VI_ERROR_RSRC_NFOUND": "error_resource_not_found", "VI_ERROR_INV_RSRC_NAME": "error_invalid_resource_name",
 "VI_ERROR_INV_ACC_MODE": "error_invalid_access_mode", "VI_ERROR_TMO": "error_timeout",
 "VI_ERROR_NSUP_ATTR": "error_nonsupported_attribute", "VI_ERROR_NSUP_ATTR_STATE": "error_nonsupported_attribute_state",
 "VI_ERROR_ATTR_READONLY": "error_attribute_read_only", "VI_ERROR_IO": "error_io",
 "VI_ERROR_NSUP_MODE": "error_nonsupported_mode", "VI_ERROR_NLISTENERS": "error_no_listeners",
 "VI_ERROR_NSUP_OPER": "error_nonsupported_operation", "VI_ERROR_RSRC_BUSY": "error_resource_busy",
 "VI_ERROR_INV_PARAMETER": "error_invalid_parameter", "VI_ERROR_CONN_LOST": "error_connection_lost",
 "VI_ATTR_RSRC_CLASS": "resource_class", "VI_ATTR_RSRC_NAME": "resource_name",
 "VI_ATTR_RSRC_IMPL_VERSION": "resource_impl_version", "VI_ATTR_RSRC_LOCK_STATE": "resource_lock_state",
 "VI_ATTR_MAX_QUEUE_LENGTH": "max_queue_length", "VI_ATTR_USER_DATA_32": None, "VI_ATTR_USER_DATA_64": "user_data",
 "VI_ATTR_SEND_END_EN": "send_end_enabled", "VI_ATTR_TERMCHAR": "termchar", "VI_ATTR_TMO_VALUE": "timeout_value",
 "VI_ATTR_GPIB_READDR_EN": "gpib_readdress_enabled", "VI_ATTR_TERMCHAR_EN": "termchar_enabled",
 "VI_ATTR_RSRC_SPEC_VERSION": "resource_spec_version", "VI_ATTR_INTF_TYPE": "interface_type",
 "VI_ATTR_GPIB_PRIMARY_ADDR": "gpib_primary_address", "VI_ATTR_GPIB_SECONDARY_ADDR": "gpib_secondary_address",
 "VI_ATTR_RSRC_MANF_NAME": "resource_manufacturer_name", "VI_ATTR_INTF_NUM": "interface_number",
 "VI_ATTR_GPIB_REN_STATE": "gpib_ren_state", "VI_ATTR_GPIB_ATN_STATE": "gpib_atn_state", "VI_ATTR_GPIB_CIC_STATE": "gpib_cic_state",
 "VI_ATTR_GPIB_NDAC_STATE": "gpib_ndac_state", "VI_ATTR_GPIB_SRQ_STATE": "gpib_srq_state", "VI_ATTR_GPIB_SYS_CNTRL_STATE": None, "VI_ATTR_INTF_INST_NAME": "interface_instrument_name",
 "VI_SUCCESS_EVENT_EN": "success_event_already_enabled", "VI_ERROR_INV_EVENT": "error_invalid_event",
 "VI_ERROR_INV_MECH": "error_invalid_mechanism", "VI_ERROR_HNDLR_NINSTALLED": "error_handler_not_installed",
 "VI_ERROR_INV_HNDLR_REF": "error_invalid_handler_reference", "VI_ERROR_NOT_ENABLED": "error_not_enabled",
 "VI_ERROR_ABORT": "error_abort", "VI_ATTR_SUPPRESS_END_EN": "suppress_end_enabled",
 "VI_ATTR_TCPIP_ADDR": "tcpip_address", "VI_ATTR_TCPIP_HOSTNAME": "tcpip_hostname", "VI_ATTR_TCPIP_PORT": "tcpip_port",
 "VI_ATTR_TCPIP_DEVICE_NAME": "tcpip_device_name", "VI_ATTR_TCPIP_IS_HISLIP": "tcpip_is_hislip",
 "VI_GPIB_REN_DEASSERT": "ren.deassert", "VI_GPIB_REN_ASSERT": "ren.asrt", "VI_GPIB_REN_DEASSERT_GTL": "ren.deassert_gtl",
 "VI_GPIB_REN_ASSERT_ADDRESS": "ren.asrt_address", "VI_GPIB_REN_ASSERT_LLO": "ren.asrt_llo",
 "VI_GPIB_REN_ASSERT_ADDRESS_LLO": "ren.asrt_address_llo", "VI_GPIB_REN_ADDRESS_GTL": "ren.address_gtl",
 "VI_ATTR_MANF_ID": "manufacturer_id", "VI_ATTR_MODEL_CODE": "model_code", "VI_ATTR_MANF_NAME": "manufacturer_name",
 "VI_ATTR_MODEL_NAME": "model_name", "VI_ATTR_USB_SERIAL_NUM": "usb_serial_number", "VI_ATTR_USB_INTFC_NUM": "usb_interface_number",
 "VI_ATTR_USB_PROTOCOL": "usb_protocol", "VI_ATTR_4882_COMPLIANT": "is_4882_compliant",
 "VI_ATTR_IO_PROT": "io_prot", "VI_ATTR_ASRL_BAUD": "asrl_baud_rate", "VI_ATTR_ASRL_DATA_BITS": "asrl_data_bits",
 "VI_ATTR_ASRL_PARITY": "asrl_parity", "VI_ATTR_ASRL_STOP_BITS": "asrl_stop_bits", "VI_ATTR_ASRL_FLOW_CNTRL": "asrl_flow_control",
 "VI_ATTR_ASRL_AVAIL_NUM": "asrl_avalaible_number", "VI_ATTR_ASRL_CTS_STATE": "asrl_cts_state", "VI_ATTR_ASRL_DCD_STATE": "asrl_dcd_state",
 "VI_ATTR_ASRL_DISCARD_NULL": "asrl_discard_null", "VI_ATTR_ASRL_DSR_STATE": "asrl_dsr_state", "VI_ATTR_ASRL_DTR_STATE": "asrl_dtr_state",
 "VI_ATTR_ASRL_END_IN": "asrl_end_in", "VI_ATTR_ASRL_END_OUT": "asrl_end_out", "VI_ATTR_ASRL_REPLACE_CHAR": "asrl_replace_char",
 "VI_ATTR_ASRL_RI_STATE": "asrl_ri_state", "VI_ATTR_ASRL_RTS_STATE": "asrl_rts_state", "VI_ATTR_ASRL_XON_CHAR": "asrl_xon_char",
 "VI_ATTR_ASRL_XOFF_CHAR": "asrl_xoff_char", "VI_ATTR_ASRL_BREAK_STATE": "asrl_break_state", "VI_ATTR_ASRL_BREAK_LEN": "asrl_break_length",
 "VI_ATTR_ASRL_ALLOW_TRANSMIT": "asrl_allow_transmit",
 "VI_EVENT_SERVICE_REQ": "service_request", "VI_ALL_ENABLED_EVENTS": "all_enabled", "VI_ATTR_EVENT_TYPE": "event_type",
}
src = open(sys.argv[1] if len(sys.argv) > 1 else "crates/symbiotic-visa/src/consts.rs").read()
ours = {}
for m in re.finditer(r"(VI_[A-Z0-9_]+)(?::\s*\w+)?\s*=\s*(0x[0-9A-Fa-f]+|[0-9]+)\b", src):
    ours[m.group(1)] = int(m.group(2), 0) & 0xFFFFFFFF
theirs = {}
for enum in (c.StatusCode, c.ResourceAttribute, c.EventType, c.EventAttribute):
    for mem in enum:
        theirs.setdefault(mem.name, int(mem.value) & 0xFFFFFFFF)
# Small numbers, whose member names could collide with the others': kept under a prefix.
for mem in c.RENLineOperation:
    theirs["ren." + mem.name] = int(mem.value)
bad = 0
for n, v in sorted(ours.items()):
    if n not in NAMES:
        # A plain number (VI_ASRL_END_TERMCHAR, VI_IO_IN_BUF, ...): pyvisa has it under the same name.
        if hasattr(c, n):
            ok = int(getattr(c, n)) & 0xFFFFFFFF == v
            bad += not ok
            print("%-4s %-28s 0x%08X  pyvisa %s = %s" % ("ok" if ok else "BAD", n, v, n, hex(int(getattr(c, n)) & 0xFFFFFFFF)))
        continue
    pn = NAMES[n]
    if pn is None:                    # pyvisa has no 32-bit user-data member; value checked by hand
        continue
    ok = pn in theirs and theirs[pn] == v
    bad += not ok
    print("%-4s %-28s 0x%08X  pyvisa %s = %s" % ("ok" if ok else "BAD", n, v, pn, hex(theirs[pn]) if pn in theirs else "missing"))
print("%d mismatch(es)" % bad)
sys.exit(bad)
