// java/src/main/java/dev/gabion/telephony/protocol/ProtocolJson.java

package dev.gabion.telephony.protocol;

import java.io.IOException;
import java.io.InputStream;
import java.util.HashMap;
import java.util.Map;
import java.util.regex.Pattern;
import tools.jackson.core.JacksonException;
import tools.jackson.databind.JsonNode;
import tools.jackson.databind.json.JsonMapper;

/**
 * Decoding and encoding of protocol messages: the description judges, the types carry.
 *
 * <p>A message is first checked against the protocol description itself and only then turned
 * into its type. The types alone are not the judge: a mapper lets through things the description
 * forbids — an extra field, a number written as text, {@code null} for a field that may only be
 * absent — and which of them it lets through depends on how it happens to be configured. With
 * the description as the judge, what it says is what is accepted. The controller decodes the
 * same way, and both sides are held to the same vectors.</p>
 *
 * <p>The checker understands exactly the words the description is allowed to use and refuses
 * any other, so it cannot silently skip a rule.</p>
 */
public final class ProtocolJson {

    private static final String DESCRIPTION_RESOURCE = "node-protocol.schema.json";
    private static final String REFERENCE_PREFIX = "#/definitions/";

    private static final JsonMapper MAPPER = JsonMapper.builder().build();
    private static final JsonNode DEFINITIONS = loadDefinitions();
    private static final Map<String, Pattern> PATTERNS = new HashMap<>();

    private ProtocolJson() {}

    /**
     * Decode a protocol message from its JSON text.
     *
     * @param json the text
     * @param type a type generated from the description; its simple name is the definition's
     * @return the message
     * @throws ProtocolViolation when the text is not a message the description allows — nothing
     *                           is skipped and nothing is defaulted
     */
    public static <T> T decode(String json, Class<T> type) throws ProtocolViolation {
        JsonNode value;
        try {
            value = MAPPER.readTree(json);
        } catch (JacksonException e) {
            throw new ProtocolViolation("", "is not JSON: " + e.getOriginalMessage());
        }
        return decode(value, type);
    }

    /**
     * Decode a protocol message from a JSON value.
     *
     * @param value the value
     * @param type  a type generated from the description
     * @return the message
     * @throws ProtocolViolation when the value is not a message the description allows
     */
    public static <T> T decode(JsonNode value, Class<T> type) throws ProtocolViolation {
        JsonNode schema = DEFINITIONS.get(type.getSimpleName());
        if (schema == null) {
            throw new IllegalArgumentException(
                    type.getSimpleName() + " is not a type of the protocol description");
        }
        check(schema, value, "");
        try {
            return MAPPER.treeToValue(value, type);
        } catch (JacksonException e) {
            // The value satisfies the description but its type refuses it: the description and
            // the generated types disagree. That is a defect of the generation, not of the sender.
            throw new IllegalStateException(
                    "a message that satisfies the protocol description was refused by "
                            + type.getSimpleName(), e);
        }
    }

    /**
     * Encode a protocol message as JSON text.
     *
     * @param message a value of a type generated from the description
     * @return the text
     */
    public static String encode(Object message) {
        return MAPPER.writeValueAsString(message);
    }

    private static JsonNode loadDefinitions() {
        try (InputStream stream = ProtocolJson.class.getResourceAsStream(DESCRIPTION_RESOURCE)) {
            if (stream == null) {
                throw new IllegalStateException(
                        "the protocol description " + DESCRIPTION_RESOURCE + " is not next to "
                                + ProtocolJson.class.getName());
            }
            JsonNode definitions = MAPPER.readTree(stream).get("definitions");
            if (definitions == null || !definitions.isObject()) {
                throw new IllegalStateException("the protocol description has no definitions");
            }
            return definitions;
        } catch (IOException e) {
            throw new IllegalStateException("the protocol description cannot be read", e);
        }
    }

    private static void check(JsonNode schema, JsonNode value, String at) throws ProtocolViolation {
        for (Map.Entry<String, JsonNode> entry : schema.properties()) {
            JsonNode rule = entry.getValue();
            switch (entry.getKey()) {
                // Words that say nothing about a value, and the two read together with "properties".
                case "description", "title", "$schema", "definitions", "required",
                        "additionalProperties" -> { }
                case "$ref" -> check(definition(rule.stringValue()), value, at);
                case "type" -> checkType(rule.stringValue(), value, at);
                case "const" -> {
                    if (!rule.equals(value)) {
                        throw new ProtocolViolation(at, "must be " + rule);
                    }
                }
                case "enum" -> {
                    boolean allowed = false;
                    for (JsonNode candidate : rule) {
                        allowed |= candidate.equals(value);
                    }
                    if (!allowed) {
                        throw new ProtocolViolation(at, value + " is not one of the allowed values");
                    }
                }
                case "minLength" -> {
                    if (value.isString() && length(value) < rule.longValue()) {
                        throw new ProtocolViolation(
                                at, "is shorter than " + rule.longValue() + " characters");
                    }
                }
                case "maxLength" -> {
                    if (value.isString() && length(value) > rule.longValue()) {
                        throw new ProtocolViolation(
                                at, "is longer than " + rule.longValue() + " characters");
                    }
                }
                case "minimum" -> {
                    if (value.isIntegralNumber() && value.canConvertToLong()
                            && value.longValue() < rule.longValue()) {
                        throw new ProtocolViolation(
                                at, value.longValue() + " is less than " + rule.longValue());
                    }
                }
                case "pattern" -> {
                    if (value.isString() && !pattern(rule.stringValue())
                            .matcher(value.stringValue()).find()) {
                        throw new ProtocolViolation(at, "is not of the required form");
                    }
                }
                case "items" -> {
                    if (value.isArray()) {
                        int index = 0;
                        for (JsonNode item : value) {
                            check(rule, item, at + "/" + index++);
                        }
                    }
                }
                case "properties" -> checkProperties(schema, rule, value, at);
                case "oneOf" -> checkOneOf(rule, value, at);
                default -> throw new IllegalStateException(
                        "the protocol description uses `" + entry.getKey()
                                + "`, which this checker does not enforce");
            }
        }
    }

    private static void checkProperties(JsonNode schema, JsonNode properties, JsonNode value,
            String at) throws ProtocolViolation {
        if (!value.isObject()) {
            return;
        }
        JsonNode required = schema.get("required");
        if (required != null) {
            for (JsonNode name : required) {
                if (!value.has(name.stringValue())) {
                    throw new ProtocolViolation(at, "`" + name.stringValue() + "` is missing");
                }
            }
        }
        // An object rule without the closing word would let anything through.
        JsonNode closed = schema.get("additionalProperties");
        if (closed == null || !closed.isBoolean() || closed.booleanValue()) {
            throw new IllegalStateException(
                    "the object rule for \"" + at + "\" does not close its set of fields");
        }
        for (Map.Entry<String, JsonNode> field : value.properties()) {
            JsonNode rule = properties.get(field.getKey());
            if (rule == null) {
                throw new ProtocolViolation(
                        at, "`" + field.getKey() + "` is not a field the protocol knows");
            }
            check(rule, field.getValue(), at + "/" + field.getKey());
        }
    }

    /**
     * Exactly one alternative must hold. For the tagged alternatives the description uses, the
     * tag picks the one whose refusal is worth reporting.
     */
    private static void checkOneOf(JsonNode alternatives, JsonNode value, String at)
            throws ProtocolViolation {
        int holding = 0;
        ProtocolViolation reported = null;
        for (JsonNode alternative : alternatives) {
            try {
                check(alternative, value, at);
                holding++;
            } catch (ProtocolViolation violation) {
                JsonNode tag = alternative.at("/properties/type/const");
                if (!tag.isMissingNode() && tag.equals(value.get("type"))) {
                    reported = violation;
                }
            }
        }
        if (holding == 1) {
            return;
        }
        if (holding > 1) {
            throw new IllegalStateException("alternatives for \"" + at + "\" are not exclusive");
        }
        throw reported != null
                ? reported
                : new ProtocolViolation(at, "is of no kind the protocol knows");
    }

    private static void checkType(String type, JsonNode value, String at) throws ProtocolViolation {
        boolean holds = switch (type) {
            case "object" -> value.isObject();
            case "array" -> value.isArray();
            case "string" -> value.isString();
            // A whole number written as a JSON integer: 1.5 and 1.0 are not one.
            case "integer" -> value.isIntegralNumber() && value.canConvertToLong();
            default -> throw new IllegalStateException(
                    "type " + type + " is not one this checker knows");
        };
        if (!holds) {
            throw new ProtocolViolation(at, "must be of type " + type);
        }
    }

    private static long length(JsonNode text) {
        String string = text.stringValue();
        return string.codePointCount(0, string.length());
    }

    private static JsonNode definition(String reference) {
        if (!reference.startsWith(REFERENCE_PREFIX)) {
            throw new IllegalStateException("bad reference " + reference);
        }
        JsonNode definition = DEFINITIONS.get(reference.substring(REFERENCE_PREFIX.length()));
        if (definition == null) {
            throw new IllegalStateException("the protocol description does not define " + reference);
        }
        return definition;
    }

    private static synchronized Pattern pattern(String expression) {
        return PATTERNS.computeIfAbsent(expression, Pattern::compile);
    }
}
