#version 300 es
precision highp float;

in vec2 uv;
uniform sampler2D desktop_image;
uniform sampler2D cursor_image;
uniform bool cursor_visible;
uniform vec4 cursor_destination;
uniform vec4 cursor_source;
out vec4 color;

void main() {
    vec3 background = texture(desktop_image, uv).rgb;
    if (cursor_visible) {
        vec2 relative = (gl_FragCoord.xy - cursor_destination.xy) / cursor_destination.zw;
        if (all(greaterThanEqual(relative, vec2(0.0))) && all(lessThan(relative, vec2(1.0)))) {
            vec4 cursor = texture(cursor_image, cursor_source.xy + relative * cursor_source.zw);
            background = cursor.rgb + background * (1.0 - cursor.a);
        }
    }
    color = vec4(background, 1.0);
}
