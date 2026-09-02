/// An HTTP status code, kept as a plain `u16` so any code can be expressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Status(pub u16);

impl Status {
    /// The handshake that hands a connection to another protocol.
    pub const SWITCHING_PROTOCOLS: Status = Status(101);
    pub const OK: Status = Status(200);
    pub const CREATED: Status = Status(201);
    pub const NO_CONTENT: Status = Status(204);
    pub const FOUND: Status = Status(302);
    pub const SEE_OTHER: Status = Status(303);
    pub const NOT_MODIFIED: Status = Status(304);
    pub const BAD_REQUEST: Status = Status(400);
    pub const UNAUTHORIZED: Status = Status(401);
    pub const FORBIDDEN: Status = Status(403);
    pub const NOT_FOUND: Status = Status(404);
    pub const METHOD_NOT_ALLOWED: Status = Status(405);
    pub const CONFLICT: Status = Status(409);
    pub const PAYLOAD_TOO_LARGE: Status = Status(413);
    /// Laravel's status for a rejected CSRF token: the form was rendered too
    /// long ago, or in another session.
    pub const PAGE_EXPIRED: Status = Status(419);
    pub const UNPROCESSABLE: Status = Status(422);
    pub const TOO_MANY_REQUESTS: Status = Status(429);
    pub const INTERNAL_ERROR: Status = Status(500);
    pub const SERVICE_UNAVAILABLE: Status = Status(503);

    pub fn code(self) -> u16 {
        self.0
    }

    pub fn is_success(self) -> bool {
        (200..300).contains(&self.0)
    }

    pub fn is_redirect(self) -> bool {
        (300..400).contains(&self.0)
    }

    pub fn is_error(self) -> bool {
        self.0 >= 400
    }

    /// Responses with these statuses must not carry a body.
    pub fn is_bodyless(self) -> bool {
        matches!(self.0, 204 | 304) || (100..200).contains(&self.0)
    }

    pub fn reason(self) -> &'static str {
        match self.0 {
            100 => "Continue",
            101 => "Switching Protocols",
            200 => "OK",
            201 => "Created",
            202 => "Accepted",
            204 => "No Content",
            301 => "Moved Permanently",
            302 => "Found",
            303 => "See Other",
            304 => "Not Modified",
            307 => "Temporary Redirect",
            308 => "Permanent Redirect",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            408 => "Request Timeout",
            409 => "Conflict",
            413 => "Payload Too Large",
            415 => "Unsupported Media Type",
            419 => "Page Expired",
            422 => "Unprocessable Content",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            501 => "Not Implemented",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            504 => "Gateway Timeout",
            _ => "Unknown",
        }
    }
}

impl From<u16> for Status {
    fn from(code: u16) -> Self {
        Status(code)
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.0, self.reason())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_status_the_framework_sends_has_a_reason_phrase() {
        // A missing arm renders as "101 Unknown" on the wire, which is what a
        // WebSocket handshake looked like before this was noticed.
        for status in [
            Status::SWITCHING_PROTOCOLS,
            Status::OK,
            Status::CREATED,
            Status::NO_CONTENT,
            Status::FOUND,
            Status::SEE_OTHER,
            Status::NOT_MODIFIED,
            Status::BAD_REQUEST,
            Status::UNAUTHORIZED,
            Status::FORBIDDEN,
            Status::NOT_FOUND,
            Status::METHOD_NOT_ALLOWED,
            Status::PAYLOAD_TOO_LARGE,
            Status::PAGE_EXPIRED,
            Status::UNPROCESSABLE,
            Status::TOO_MANY_REQUESTS,
            Status::INTERNAL_ERROR,
            Status::SERVICE_UNAVAILABLE,
        ] {
            assert_ne!(status.reason(), "Unknown", "{} has no reason phrase", status.code());
        }
    }

    #[test]
    fn an_informational_status_carries_no_body() {
        assert!(Status::SWITCHING_PROTOCOLS.is_bodyless());
        assert!(Status::NO_CONTENT.is_bodyless());
        assert!(!Status::OK.is_bodyless());
    }
}
