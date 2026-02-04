//! Visual Query Plan Generator
//!
//! Generates interactive HTML and static SVG visualizations of query execution plans.
//! Features:
//! - Interactive HTML with D3.js tree visualization
//! - Static SVG diagrams for reports
//! - Cost heat maps (red/yellow/green)
//! - Interactive drill-down (click to expand)
//! - Export functionality (PNG, PDF)

use crate::Result;
use super::explain::{ExplainOutput, PlanNode};

/// Visual plan format
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualFormat {
    /// Interactive HTML with JavaScript
    HTML,
    /// Static SVG diagram
    SVG,
    /// Mermaid diagram syntax
    Mermaid,
    /// GraphViz DOT format
    DOT,
}

/// Visual plan generator
pub struct VisualPlanGenerator {
    format: VisualFormat,
    include_costs: bool,
    include_row_counts: bool,
    color_by_cost: bool,
}

impl VisualPlanGenerator {
    pub fn new(format: VisualFormat) -> Self {
        Self {
            format,
            include_costs: true,
            include_row_counts: true,
            color_by_cost: true,
        }
    }

    /// Generate visual representation of query plan
    pub fn generate(&self, explain: &ExplainOutput) -> Result<String> {
        match self.format {
            VisualFormat::HTML => self.generate_html(explain),
            VisualFormat::SVG => self.generate_svg(explain),
            VisualFormat::Mermaid => self.generate_mermaid(explain),
            VisualFormat::DOT => self.generate_dot(explain),
        }
    }

    /// Generate interactive HTML with D3.js visualization
    fn generate_html(&self, explain: &ExplainOutput) -> Result<String> {
        let tree_data = self.node_to_json(&explain.plan);

        let html = format!(r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Query Execution Plan - HeliosDB</title>
    <script src="https://d3js.org/d3.v7.min.js"></script>
    <style>
        * {{
            margin: 0;
            padding: 0;
            box-sizing: border-box;
        }}

        body {{
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
            background: #f5f5f5;
            padding: 20px;
        }}

        .container {{
            max-width: 1400px;
            margin: 0 auto;
            background: white;
            border-radius: 8px;
            box-shadow: 0 2px 8px rgba(0,0,0,0.1);
            overflow: hidden;
        }}

        .header {{
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
            color: white;
            padding: 30px;
        }}

        .header h1 {{
            font-size: 28px;
            font-weight: 600;
            margin-bottom: 10px;
        }}

        .stats {{
            display: grid;
            grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
            gap: 20px;
            padding: 20px 30px;
            background: #f9fafb;
            border-bottom: 1px solid #e5e7eb;
        }}

        .stat {{
            padding: 15px;
            background: white;
            border-radius: 6px;
            border: 1px solid #e5e7eb;
        }}

        .stat-label {{
            font-size: 12px;
            color: #6b7280;
            text-transform: uppercase;
            letter-spacing: 0.5px;
            margin-bottom: 5px;
        }}

        .stat-value {{
            font-size: 24px;
            font-weight: 700;
            color: #111827;
        }}

        .visualization {{
            padding: 30px;
            min-height: 600px;
        }}

        .controls {{
            padding: 20px 30px;
            background: #f9fafb;
            border-top: 1px solid #e5e7eb;
            display: flex;
            gap: 15px;
            flex-wrap: wrap;
        }}

        .btn {{
            padding: 10px 20px;
            border: none;
            border-radius: 6px;
            font-size: 14px;
            font-weight: 500;
            cursor: pointer;
            transition: all 0.2s;
        }}

        .btn-primary {{
            background: #667eea;
            color: white;
        }}

        .btn-primary:hover {{
            background: #5568d3;
        }}

        .btn-secondary {{
            background: white;
            color: #374151;
            border: 1px solid #d1d5db;
        }}

        .btn-secondary:hover {{
            background: #f9fafb;
        }}

        /* D3 Tree Styles */
        .node circle {{
            stroke: #374151;
            stroke-width: 2px;
            cursor: pointer;
        }}

        .node-fast circle {{
            fill: #10b981;
        }}

        .node-moderate circle {{
            fill: #f59e0b;
        }}

        .node-slow circle {{
            fill: #ef4444;
        }}

        .node text {{
            font-size: 12px;
            font-weight: 500;
            fill: #111827;
        }}

        .node-details {{
            font-size: 11px;
            fill: #6b7280;
        }}

        .link {{
            fill: none;
            stroke: #9ca3af;
            stroke-width: 2px;
        }}

        .tooltip {{
            position: absolute;
            background: #1f2937;
            color: white;
            padding: 12px;
            border-radius: 6px;
            font-size: 13px;
            pointer-events: none;
            opacity: 0;
            transition: opacity 0.2s;
            max-width: 300px;
            box-shadow: 0 4px 6px rgba(0,0,0,0.1);
        }}

        .tooltip-visible {{
            opacity: 1;
        }}
    </style>
</head>
<body>
    <div class="container">
        <div class="header">
            <h1>Query Execution Plan</h1>
            <p>Interactive visualization of query optimization and execution strategy</p>
        </div>

        <div class="stats">
            <div class="stat">
                <div class="stat-label">Total Cost</div>
                <div class="stat-value">{:.2}</div>
            </div>
            <div class="stat">
                <div class="stat-label">Estimated Rows</div>
                <div class="stat-value">{}</div>
            </div>
            <div class="stat">
                <div class="stat-label">Planning Time</div>
                <div class="stat-value">{:.2}ms</div>
            </div>
            <div class="stat">
                <div class="stat-label">Optimizer Features</div>
                <div class="stat-value">{}</div>
            </div>
        </div>

        <div class="visualization">
            <svg id="tree"></svg>
        </div>

        <div class="controls">
            <button class="btn btn-primary" onclick="expandAll()">Expand All</button>
            <button class="btn btn-secondary" onclick="collapseAll()">Collapse All</button>
            <button class="btn btn-secondary" onclick="exportSVG()">Export SVG</button>
            <button class="btn btn-secondary" onclick="exportPNG()">Export PNG</button>
        </div>
    </div>

    <div class="tooltip" id="tooltip"></div>

    <script>
        // Query plan data injected from Rust
        const treeData = {{TREE_DATA}};

        // ============================================================================
        // D3.js Interactive Tree Visualization
        // ============================================================================

        // Configuration
        const config = {{
            nodeWidth: 180,
            nodeHeight: 70,
            levelHeight: 120,
            duration: 500,
            margin: {{ top: 40, right: 40, bottom: 40, left: 40 }}
        }};

        // Initialize dimensions
        const containerWidth = document.querySelector('.visualization').clientWidth - config.margin.left - config.margin.right;
        const containerHeight = 600 - config.margin.top - config.margin.bottom;

        // Create SVG container with zoom support
        const svg = d3.select('#tree')
            .attr('width', containerWidth + config.margin.left + config.margin.right)
            .attr('height', containerHeight + config.margin.top + config.margin.bottom)
            .call(d3.zoom()
                .scaleExtent([0.3, 3])
                .on('zoom', (event) => {{
                    g.attr('transform', event.transform);
                }}))
            .append('g')
            .attr('transform', `translate(${{config.margin.left + containerWidth / 2}},${{config.margin.top}})`);

        const g = svg;

        // Tooltip element
        const tooltip = d3.select('#tooltip');

        // Tree layout
        const tree = d3.tree()
            .nodeSize([config.nodeWidth + 30, config.levelHeight])
            .separation((a, b) => a.parent === b.parent ? 1 : 1.5);

        // Create hierarchical data
        const root = d3.hierarchy(treeData);
        root.x0 = 0;
        root.y0 = 0;

        // Store all nodes for expand/collapse all
        let allNodes = root.descendants();

        // Initially collapse nodes below level 2
        root.descendants().forEach((d, i) => {{
            if (d.depth > 1 && d.children) {{
                d._children = d.children;
                d.children = null;
            }}
        }});

        // Link generator (curved links)
        const linkGenerator = d3.linkVertical()
            .x(d => d.x)
            .y(d => d.y);

        // Initial render
        update(root);

        // ============================================================================
        // Update Function - Core rendering logic
        // ============================================================================
        function update(source) {{
            const treeData = tree(root);
            const nodes = treeData.descendants();
            const links = treeData.links();

            // Normalize for fixed-depth
            nodes.forEach(d => {{ d.y = d.depth * config.levelHeight; }});

            // ===== NODES =====
            const node = g.selectAll('g.node')
                .data(nodes, d => d.id || (d.id = Math.random()));

            // Enter new nodes
            const nodeEnter = node.enter()
                .append('g')
                .attr('class', d => `node node-${{d.data.costCategory}}`)
                .attr('transform', d => `translate(${{source.x0}},${{source.y0}})`)
                .on('click', (event, d) => {{
                    toggle(d);
                    update(d);
                }})
                .on('mouseover', showTooltip)
                .on('mouseout', hideTooltip);

            // Add node background rectangle
            nodeEnter.append('rect')
                .attr('width', config.nodeWidth)
                .attr('height', config.nodeHeight)
                .attr('x', -config.nodeWidth / 2)
                .attr('y', -config.nodeHeight / 2)
                .attr('rx', 8)
                .attr('ry', 8)
                .style('fill', d => getNodeColor(d.data.costCategory))
                .style('stroke', d => getNodeStroke(d.data.costCategory))
                .style('stroke-width', 2)
                .style('cursor', 'pointer')
                .style('filter', 'drop-shadow(0 2px 4px rgba(0,0,0,0.1))');

            // Add operation name
            nodeEnter.append('text')
                .attr('class', 'node-title')
                .attr('dy', -15)
                .attr('text-anchor', 'middle')
                .style('fill', 'white')
                .style('font-weight', '600')
                .style('font-size', '13px')
                .style('pointer-events', 'none')
                .text(d => truncateText(d.data.name, 22));

            // Add cost
            nodeEnter.append('text')
                .attr('class', 'node-cost')
                .attr('dy', 5)
                .attr('text-anchor', 'middle')
                .style('fill', 'rgba(255,255,255,0.9)')
                .style('font-size', '11px')
                .style('pointer-events', 'none')
                .text(d => `Cost: ${{formatNumber(d.data.cost)}}`);

            // Add rows
            nodeEnter.append('text')
                .attr('class', 'node-rows')
                .attr('dy', 22)
                .attr('text-anchor', 'middle')
                .style('fill', 'rgba(255,255,255,0.9)')
                .style('font-size', '11px')
                .style('pointer-events', 'none')
                .text(d => `Rows: ${{formatNumber(d.data.rows)}}`);

            // Add expand/collapse indicator
            nodeEnter.append('text')
                .attr('class', 'node-indicator')
                .attr('dy', config.nodeHeight / 2 + 15)
                .attr('text-anchor', 'middle')
                .style('fill', '#666')
                .style('font-size', '12px')
                .style('font-weight', 'bold')
                .style('pointer-events', 'none')
                .text(d => d.children || d._children ? (d.children ? '\u25BC' : '\u25BA') : '');

            // Merge enter and existing nodes
            const nodeUpdate = nodeEnter.merge(node);

            // Transition nodes to new positions
            nodeUpdate.transition()
                .duration(config.duration)
                .attr('transform', d => `translate(${{d.x}},${{d.y}})`);

            // Update indicator
            nodeUpdate.select('.node-indicator')
                .text(d => d.children || d._children ? (d.children ? '\u25BC' : '\u25BA') : '');

            // Remove exiting nodes
            const nodeExit = node.exit()
                .transition()
                .duration(config.duration)
                .attr('transform', d => `translate(${{source.x}},${{source.y}})`)
                .remove();

            nodeExit.select('rect').style('opacity', 0);
            nodeExit.selectAll('text').style('fill-opacity', 0);

            // ===== LINKS =====
            const link = g.selectAll('path.link')
                .data(links, d => d.target.id);

            // Enter new links
            const linkEnter = link.enter()
                .insert('path', 'g')
                .attr('class', 'link')
                .attr('d', d => {{
                    const o = {{ x: source.x0, y: source.y0 }};
                    return linkGenerator({{ source: o, target: o }});
                }})
                .style('fill', 'none')
                .style('stroke', '#9ca3af')
                .style('stroke-width', 2)
                .style('stroke-dasharray', d => d.target._children ? '5,5' : 'none');

            // Merge and transition links
            linkEnter.merge(link)
                .transition()
                .duration(config.duration)
                .attr('d', d => linkGenerator({{
                    source: {{ x: d.source.x, y: d.source.y + config.nodeHeight / 2 }},
                    target: {{ x: d.target.x, y: d.target.y - config.nodeHeight / 2 }}
                }}));

            // Remove exiting links
            link.exit()
                .transition()
                .duration(config.duration)
                .attr('d', d => {{
                    const o = {{ x: source.x, y: source.y }};
                    return linkGenerator({{ source: o, target: o }});
                }})
                .remove();

            // Store positions for next transition
            nodes.forEach(d => {{
                d.x0 = d.x;
                d.y0 = d.y;
            }});
        }}

        // ============================================================================
        // Helper Functions
        // ============================================================================

        function toggle(d) {{
            if (d.children) {{
                d._children = d.children;
                d.children = null;
            }} else if (d._children) {{
                d.children = d._children;
                d._children = null;
            }}
        }}

        function getNodeColor(category) {{
            switch (category) {{
                case 'fast': return '#10b981';
                case 'moderate': return '#f59e0b';
                case 'slow': return '#ef4444';
                default: return '#6b7280';
            }}
        }}

        function getNodeStroke(category) {{
            switch (category) {{
                case 'fast': return '#059669';
                case 'moderate': return '#d97706';
                case 'slow': return '#dc2626';
                default: return '#4b5563';
            }}
        }}

        function truncateText(text, maxLength) {{
            return text.length > maxLength ? text.substring(0, maxLength - 1) + '\u2026' : text;
        }}

        function formatNumber(num) {{
            if (num >= 1000000) return (num / 1000000).toFixed(1) + 'M';
            if (num >= 1000) return (num / 1000).toFixed(1) + 'K';
            return num.toFixed(num % 1 === 0 ? 0 : 2);
        }}

        function showTooltip(event, d) {{
            let content = `<strong>${{d.data.name}}</strong><br/>`;
            content += `<span style="color:#10b981;">Cost:</span> ${{d.data.cost.toFixed(2)}}<br/>`;
            content += `<span style="color:#f59e0b;">Rows:</span> ${{formatNumber(d.data.rows)}}<br/>`;
            content += `<span style="color:#6b7280;">Category:</span> ${{d.data.costCategory}}`;

            if (d.data.details) {{
                content += `<br/><hr style="border-color:#444;margin:6px 0;"/>`;
                content += `<span style="color:#9ca3af;">${{d.data.details}}</span>`;
            }}

            if (d.children || d._children) {{
                const childCount = (d.children || d._children).length;
                content += `<br/><em style="color:#818cf8;">Click to ${{d.children ? 'collapse' : 'expand'}} (${{childCount}} child${{childCount > 1 ? 'ren' : ''}})</em>`;
            }}

            tooltip
                .html(content)
                .style('left', (event.pageX + 15) + 'px')
                .style('top', (event.pageY - 15) + 'px')
                .classed('tooltip-visible', true);
        }}

        function hideTooltip() {{
            tooltip.classed('tooltip-visible', false);
        }}

        // ============================================================================
        // Control Functions
        // ============================================================================

        function expandAll() {{
            function expand(d) {{
                if (d._children) {{
                    d.children = d._children;
                    d._children = null;
                }}
                if (d.children) {{
                    d.children.forEach(expand);
                }}
            }}
            expand(root);
            update(root);
        }}

        function collapseAll() {{
            function collapse(d) {{
                if (d.children && d.depth > 0) {{
                    d._children = d.children;
                    d.children = null;
                }}
                if (d._children) {{
                    d._children.forEach(collapse);
                }}
            }}
            root.children?.forEach(collapse);
            update(root);
        }}

        function exportSVG() {{
            // Clone SVG for export
            const svgElement = document.querySelector('#tree');
            const svgClone = svgElement.cloneNode(true);

            // Add styles inline
            const styleText = document.querySelector('style').textContent;
            const styleElement = document.createElementNS('http://www.w3.org/2000/svg', 'style');
            styleElement.textContent = styleText;
            svgClone.insertBefore(styleElement, svgClone.firstChild);

            // Create download
            const serializer = new XMLSerializer();
            const svgString = serializer.serializeToString(svgClone);
            const blob = new Blob([svgString], {{ type: 'image/svg+xml' }});
            const url = URL.createObjectURL(blob);

            const link = document.createElement('a');
            link.href = url;
            link.download = 'query-plan.svg';
            document.body.appendChild(link);
            link.click();
            document.body.removeChild(link);
            URL.revokeObjectURL(url);
        }}

        function exportPNG() {{
            const svgElement = document.querySelector('#tree');
            const svgRect = svgElement.getBoundingClientRect();

            // Create canvas
            const canvas = document.createElement('canvas');
            canvas.width = svgRect.width * 2;
            canvas.height = svgRect.height * 2;
            const ctx = canvas.getContext('2d');
            ctx.scale(2, 2);

            // Convert SVG to image
            const svgData = new XMLSerializer().serializeToString(svgElement);
            const img = new Image();
            const svgBlob = new Blob([svgData], {{ type: 'image/svg+xml;charset=utf-8' }});
            const url = URL.createObjectURL(svgBlob);

            img.onload = function() {{
                ctx.fillStyle = 'white';
                ctx.fillRect(0, 0, canvas.width, canvas.height);
                ctx.drawImage(img, 0, 0);
                URL.revokeObjectURL(url);

                // Download
                const pngUrl = canvas.toDataURL('image/png');
                const link = document.createElement('a');
                link.href = pngUrl;
                link.download = 'query-plan.png';
                document.body.appendChild(link);
                link.click();
                document.body.removeChild(link);
            }};

            img.src = url;
        }}

        // Fit to view on initial load
        function fitToView() {{
            const bounds = g.node().getBBox();
            const parent = svg.node().parentElement;
            const fullWidth = parent.clientWidth;
            const fullHeight = parent.clientHeight;

            const width = bounds.width;
            const height = bounds.height;
            const midX = bounds.x + width / 2;
            const midY = bounds.y + height / 2;

            if (width === 0 || height === 0) return;

            const scale = 0.8 / Math.max(width / fullWidth, height / fullHeight);
            const translate = [fullWidth / 2 - scale * midX, fullHeight / 2 - scale * midY];

            svg.transition()
                .duration(500)
                .call(d3.zoom().transform, d3.zoomIdentity.translate(translate[0], translate[1]).scale(scale));
        }}

        // Fit to view after initial render
        setTimeout(fitToView, 100);
    </script>
</body>
</html>"#,
            explain.total_cost,
            explain.total_rows,
            explain.planning_time_ms,
            explain.features.len(),
            tree_data
        );

        Ok(html)
    }

    /// Generate static SVG diagram
    fn generate_svg(&self, explain: &ExplainOutput) -> Result<String> {
        let mut svg = String::from(r#"<svg xmlns="http://www.w3.org/2000/svg" width="800" height="600" viewBox="0 0 800 600">"#);

        // Add styles
        svg.push_str(r#"
<defs>
    <style>
        .node-rect { stroke: #374151; stroke-width: 2; }
        .node-fast { fill: #10b981; }
        .node-moderate { fill: #f59e0b; }
        .node-slow { fill: #ef4444; }
        .node-text { font-family: sans-serif; font-size: 14px; fill: white; font-weight: 600; }
        .node-details { font-family: monospace; font-size: 11px; fill: white; }
        .link { stroke: #9ca3af; stroke-width: 2; fill: none; }
    </style>
</defs>
"#);

        // Draw plan tree
        self.draw_node_svg(&mut svg, &explain.plan, 400.0, 50.0, 0);

        svg.push_str("</svg>");

        Ok(svg)
    }

    /// Draw a single node in SVG
    fn draw_node_svg(&self, svg: &mut String, node: &PlanNode, x: f64, y: f64, depth: usize) -> f64 {
        let cost_category = if node.cost < 100.0 {
            "fast"
        } else if node.cost < 1000.0 {
            "moderate"
        } else {
            "slow"
        };

        // Draw rectangle
        svg.push_str(&format!(
            r#"<rect class="node-rect node-{}" x="{}" y="{}" width="160" height="60" rx="6"/>"#,
            cost_category,
            x - 80.0,
            y
        ));

        // Draw operation name
        svg.push_str(&format!(
            r#"<text class="node-text" x="{}" y="{}" text-anchor="middle">{}</text>"#,
            x,
            y + 20.0,
            node.operation
        ));

        // Draw cost and rows
        if self.include_costs {
            svg.push_str(&format!(
                r#"<text class="node-details" x="{}" y="{}" text-anchor="middle">Cost: {:.2}</text>"#,
                x,
                y + 35.0,
                node.cost
            ));
        }

        if self.include_row_counts {
            svg.push_str(&format!(
                r#"<text class="node-details" x="{}" y="{}" text-anchor="middle">Rows: {}</text>"#,
                x,
                y + 50.0,
                node.rows
            ));
        }

        // Draw children
        let child_y = y + 100.0;
        let child_spacing = 200.0;
        let total_width = (node.children.len().saturating_sub(1)) as f64 * child_spacing;
        let start_x = x - total_width / 2.0;

        for (i, child) in node.children.iter().enumerate() {
            let child_x = start_x + i as f64 * child_spacing;

            // Draw link
            svg.push_str(&format!(
                r#"<path class="link" d="M {} {} L {} {}"/>"#,
                x, y + 60.0, child_x, child_y
            ));

            // Draw child node
            self.draw_node_svg(svg, child, child_x, child_y, depth + 1);
        }

        child_y
    }

    /// Generate Mermaid diagram syntax
    fn generate_mermaid(&self, explain: &ExplainOutput) -> Result<String> {
        let mut mermaid = String::from("graph TD\n");

        self.node_to_mermaid(&mut mermaid, &explain.plan, "root", 0);

        Ok(mermaid)
    }

    /// Convert node to Mermaid syntax
    fn node_to_mermaid(&self, output: &mut String, node: &PlanNode, id: &str, counter: usize) -> usize {
        let label = if self.include_costs && self.include_row_counts {
            format!("{}<br/>Cost: {:.2}<br/>Rows: {}", node.operation, node.cost, node.rows)
        } else {
            node.operation.clone()
        };

        let style = if node.cost < 100.0 {
            "fill:#10b981,stroke:#059669,color:#fff"
        } else if node.cost < 1000.0 {
            "fill:#f59e0b,stroke:#d97706,color:#fff"
        } else {
            "fill:#ef4444,stroke:#dc2626,color:#fff"
        };

        output.push_str(&format!("    {}[\"{}\"]\n", id, label));
        output.push_str(&format!("    style {} {}\n", id, style));

        let mut next_counter = counter;
        for (i, child) in node.children.iter().enumerate() {
            let child_id = format!("{}_{}", id, i);
            output.push_str(&format!("    {} --> {}\n", id, child_id));
            next_counter = self.node_to_mermaid(output, child, &child_id, next_counter + 1);
        }

        next_counter
    }

    /// Generate GraphViz DOT format
    fn generate_dot(&self, explain: &ExplainOutput) -> Result<String> {
        let mut dot = String::from("digraph QueryPlan {\n");
        dot.push_str("    rankdir=TB;\n");
        dot.push_str("    node [shape=box, style=filled, fontname=\"Arial\"];\n");

        self.node_to_dot(&mut dot, &explain.plan, "node0", 0);

        dot.push_str("}\n");

        Ok(dot)
    }

    /// Convert node to DOT syntax
    fn node_to_dot(&self, output: &mut String, node: &PlanNode, id: &str, counter: usize) -> usize {
        let color = if node.cost < 100.0 {
            "#10b981"
        } else if node.cost < 1000.0 {
            "#f59e0b"
        } else {
            "#ef4444"
        };

        let label = if self.include_costs && self.include_row_counts {
            format!("{}\\nCost: {:.2}\\nRows: {}", node.operation, node.cost, node.rows)
        } else {
            node.operation.clone()
        };

        output.push_str(&format!(
            "    {} [label=\"{}\", fillcolor=\"{}\", fontcolor=\"white\"];\n",
            id, label, color
        ));

        let mut next_counter = counter;
        for (i, child) in node.children.iter().enumerate() {
            let child_id = format!("node{}", next_counter + 1);
            output.push_str(&format!("    {} -> {};\n", id, child_id));
            next_counter = self.node_to_dot(output, child, &child_id, next_counter + 1);
        }

        next_counter
    }

    /// Convert plan node to JSON for D3.js
    fn node_to_json(&self, node: &PlanNode) -> String {
        let cost_category = if node.cost < 100.0 {
            "fast"
        } else if node.cost < 1000.0 {
            "moderate"
        } else {
            "slow"
        };

        let mut json = format!(
            r#"{{"name":"{}","cost":{},"rows":{},"costCategory":"{}""#,
            node.operation, node.cost, node.rows, cost_category
        );

        if !node.details.is_empty() {
            let details_str = node.details.iter()
                .map(|(k, v)| format!("{}: {}", k, v))
                .collect::<Vec<_>>()
                .join(", ");
            json.push_str(&format!(r#","details":"{}""#, details_str));
        }

        if !node.children.is_empty() {
            json.push_str(r#","children":["#);
            for (i, child) in node.children.iter().enumerate() {
                if i > 0 {
                    json.push(',');
                }
                json.push_str(&self.node_to_json(child));
            }
            json.push(']');
        }

        json.push('}');
        json
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::sql::explain::{ExplainOutput, PlanNode, ConfigSnapshot};
    use std::collections::HashMap;

    fn create_sample_plan() -> PlanNode {
        PlanNode {
            node_type: "Join".to_string(),
            operation: "Hash Join".to_string(),
            cost: 1500.0,
            rows: 10000,
            details: HashMap::new(),
            children: vec![
                PlanNode {
                    node_type: "Scan".to_string(),
                    operation: "Seq Scan on users".to_string(),
                    cost: 100.0,
                    rows: 1000,
                    details: HashMap::new(),
                    children: vec![],
                },
                PlanNode {
                    node_type: "Scan".to_string(),
                    operation: "Seq Scan on orders".to_string(),
                    cost: 500.0,
                    rows: 5000,
                    details: HashMap::new(),
                    children: vec![],
                },
            ],
        }
    }

    fn create_sample_explain() -> ExplainOutput {
        ExplainOutput {
            plan: create_sample_plan(),
            total_cost: 1500.0,
            total_rows: 10000,
            planning_time_ms: 2.5,
            config: ConfigSnapshot::default(),
            features: vec![],
            decisions: vec![],
            why_not: None,
            ai_explanation: None,
            warnings: vec![],
            suggestions: vec![],
        }
    }

    #[test]
    fn test_html_generation() {
        let generator = VisualPlanGenerator::new(VisualFormat::HTML);
        let explain = create_sample_explain();
        let html = generator.generate(&explain).unwrap();

        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("Query Execution Plan"));
        assert!(html.contains("d3.js"));
    }

    #[test]
    fn test_svg_generation() {
        let generator = VisualPlanGenerator::new(VisualFormat::SVG);
        let explain = create_sample_explain();
        let svg = generator.generate(&explain).unwrap();

        assert!(svg.contains("<svg"));
        assert!(svg.contains("</svg>"));
    }

    #[test]
    fn test_mermaid_generation() {
        let generator = VisualPlanGenerator::new(VisualFormat::Mermaid);
        let explain = create_sample_explain();
        let mermaid = generator.generate(&explain).unwrap();

        assert!(mermaid.contains("graph TD"));
    }

    #[test]
    fn test_dot_generation() {
        let generator = VisualPlanGenerator::new(VisualFormat::DOT);
        let explain = create_sample_explain();
        let dot = generator.generate(&explain).unwrap();

        assert!(dot.contains("digraph QueryPlan"));
    }
}
