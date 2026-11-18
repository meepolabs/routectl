//! Background usage poll: reads each idle managed seat's usage endpoint so
//! the account meter holds a reading for seats that carry no traffic.
//! `client` is the only code that talks to a vendor usage endpoint.

#[cfg_attr(not(test), allow(dead_code))]
mod client;
