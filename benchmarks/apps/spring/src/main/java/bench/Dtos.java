package bench;

/**
 * The response shapes named by the contract. Records, so Jackson emits the
 * fields in declaration order and the bodies match byte for byte.
 */
final class Dtos {

    private Dtos() {
    }

    /** {@code {"message":"Hello, World!"}} */
    record Message(String message) {
    }

    /** {@code {"id":42,"slug":"hello-world"}} */
    record IdSlug(long id, String slug) {
    }

    /** {@code {"depth":5}} */
    record Depth(int depth) {
    }

    /** One of the hundred rows of {@code /json-big}. */
    record BigRow(int id, String name, String email, boolean active, double score) {
    }

    /** {@code {"id":42,"name":"User 42","email":"user42@example.test"}} */
    record UserRow(int id, String name, String email) {
    }

    /** The nested author of a post. */
    record Author(int id, String name) {
    }

    /** {@code {"id":1,"title":"Post 1","author":{...}}} */
    record PostRow(int id, String title, Author author) {
    }

    /** A row of the rendered table; not serialised as JSON. */
    record TableRow(int id, String name) {
    }
}
